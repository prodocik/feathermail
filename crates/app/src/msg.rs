use feathermail_core::body::BodyLookup;
use std::path::PathBuf;

use feathermail_core::{
    AccountId, Address, Attachment, AttachmentId, Density, Draft, DraftAttachment, DraftId,
    FolderSummary, McpAuditEntry, McpClientSummary, McpConfirmationChoice, McpConfirmationRequest,
    McpPermissionLevel, MessageId, OperationId, ResponseKind, SyncProgress, Theme, ThreadFilter,
    ThreadPage, UndoTicket,
};

use feathermail_service::AutoconfigOutcome;

use crate::html_view::PreparedBody;
use crate::nav::{PrefKey, SettingsPage};

/// T-156: the address one autoconfig lookup was started for, carried back
/// with its answer so a reply for a mailbox the user has since retyped can
/// be recognised and dropped.
///
/// It is a newtype rather than a `String` because [`Msg`] derives `Debug`,
/// and D14 keeps mail addresses out of every printed line -- the same
/// reason `ProvisionRequest` redacts its password and `AutoconfigOutcome`
/// redacts the form it found. Comparison is what the wizard needs, so that
/// is what the type offers; the text itself never leaves it.
#[derive(Clone, PartialEq, Eq)]
pub struct WizardEmail(String);

impl WizardEmail {
    pub fn new(email: impl Into<String>) -> Self {
        Self(email.into())
    }

    /// Whether the field still holds the address this lookup was for.
    /// Trimmed and case-insensitive, because neither difference makes it a
    /// different mailbox and both are easy to introduce by editing.
    pub fn matches(&self, current: &str) -> bool {
        self.0.trim().eq_ignore_ascii_case(current.trim())
    }
}

impl std::fmt::Debug for WizardEmail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[redacted]")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateCheck {
    Available { version: String, url: String },
    Current,
    Failed,
}

#[derive(Debug, Clone)]
pub enum Msg {
    SelectFolder(String),
    SelectThread(String),
    /// T-036: row click with the modifiers captured at the gesture edge.
    /// Keeping this separate from `SelectThread` leaves context-menu/open
    /// paths as ordinary single-thread selection.
    SelectThreadGesture {
        id: String,
        ctrl: bool,
        shift: bool,
    },
    ToggleStar(String),
    StarSelected,
    Archive,
    Delete,
    MarkUnread,
    MarkRead,
    /// T-036: open the folder picker for the current multi-selection.
    Move,
    /// T-036: move the current multi-selection to one concrete folder.
    MoveTo(String),
    /// T-036: GTK multi-selection changed (Ctrl/Shift clicks or keyboard).
    SelectionChanged,
    /// T-036: select every visible thread in the current list (Ctrl+A).
    SelectAll,
    Snooze,
    /// T-035: one of the fixed local snooze deadlines shown by the
    /// toolbar popover.  The absolute timestamp is deliberately computed
    /// in the GTK update path so the click uses one wall-clock sample.
    SnoozePreset(SnoozePreset),
    /// T-035: custom UTC datetime entered in the snooze popover as
    /// `YYYY-MM-DD HH:MM`.
    SnoozeCustom(String),
    /// T-061e: bring the selected snoozed threads back to Inbox now, exactly
    /// as their own timer would have. Only reachable from the Snoozed view,
    /// where it is the one action the list was previously missing.
    Unsnooze,
    Undo,
    HideToast {
        gen: u64,
    },
    ToggleTheme,
    /// T-097(9): warm the newest bodies of the folder on screen. Sent once
    /// per folder, after its first page has rendered.
    PrefetchFolderBodies,
    /// T-097(6): the sidebar divider moved. Fires on every pixel of a drag;
    /// the handler only updates the model and arms the debounce.
    SidebarWidth(i32),
    /// T-097(6): the debounce fired -- write the width if no further drag
    /// happened since (`gen` still matches).
    PersistSidebarWidth {
        gen: u64,
    },
    /// T-099: the list/reader divider moved. Same shape as the sidebar's, one
    /// pane over.
    ListWidth(i32),
    /// T-099: the list divider's debounce fired.
    PersistListWidth {
        gen: u64,
    },
    SetTheme(Theme),
    /// The desktop switched between its light and dark colour schemes.
    ///
    /// T-054: only "System" follows the desktop, and it has to follow it
    /// while the app is open -- a theme that needs a restart to notice is
    /// the same defect as one that never notices.
    DesktopColorSchemeChanged,
    SetDensity(Density),
    SetUiScale(u8),
    ToggleAutostart,
    ToggleMcp,
    McpEnabledChanged {
        enabled: bool,
        saved: bool,
    },
    /// T-064a: bounded policy metadata for the AI & MCP settings page. The
    /// database query runs on a worker; the page never receives grants,
    /// requests, audit payloads, or tool arguments.
    McpClientsLoad,
    McpClientsLoaded(Result<Vec<McpClientSummary>, String>),
    /// Carries an enrolled opaque id only from the Core-provided client list
    /// back to Core's revoke door. It is never rendered in UI copy or a toast.
    McpClientRevoke(String),
    McpClientRevoked(Result<bool, String>),
    /// Re-enabling a revoked local profile is an explicit, separately
    /// confirmed Settings action; the opaque id never enters rendered copy.
    McpClientReenableRequest(String),
    McpClientReenableConfirmed {
        client_id: String,
        accepted: bool,
    },
    McpClientReenabled(Result<bool, String>),
    /// Carries a Core-listed opaque id and one fixed D57 level back to the
    /// Core policy door. The page never displays the id or a raw error.
    McpClientPermissionChange {
        client_id: String,
        permission_level: McpPermissionLevel,
    },
    McpClientPermissionChanged(Result<bool, String>),
    /// T-064b: a bounded, metadata-only activity projection. It is loaded by
    /// a Core worker; account/target ids, arguments, results, and raw errors
    /// never cross into the GTK page.
    McpAuditLoad,
    McpAuditLoaded(Result<Vec<McpAuditEntry>, String>),
    /// T-059: bounded GTK-side poll of opaque Core permission requests.
    /// No tool arguments or mail text cross this message boundary.
    McpConfirmationPoll,
    McpConfirmationsLoaded(Result<Vec<McpConfirmationRequest>, String>),
    /// T-060s: the headless MCP process cannot reach the sync worker's
    /// channel, so `sync_account` leaves a durable request in Core and this
    /// window claims it on the same poll. Deliberately carries only "was
    /// there anything to claim": the wake is account-agnostic, so no
    /// account id needs to cross into GTK.
    McpSyncRequestsClaimed(bool),
    McpConfirmationRespond {
        request_id: i64,
        choice: McpConfirmationChoice,
    },
    McpConfirmationResolved {
        request_id: i64,
        accepted: bool,
    },
    SetMarkRead(crate::nav::MarkReadMode),
    TogglePref(PrefKey),
    Compose,
    CloseCompose,
    ComposeChanged,
    RecipientSuggestions {
        query: String,
        addresses: Vec<Address>,
    },
    PickRecipient(String),
    FormatCompose(&'static str),
    AutosaveDraft {
        gen: u64,
    },
    SaveDraft,
    /// `session` is the compose session that asked for the save. A window
    /// closed and reopened while the write was in flight is a different
    /// session, and this answer belongs to the previous one -- the same
    /// generation guard `BodyLookup`/`SearchResults` already carry.
    DraftSaved {
        session: u64,
        gen: u64,
        result: Result<Draft, String>,
    },
    /// T-046: completion of Core's local response-draft command. It carries
    /// metadata and user-visible draft fields only; the source body was
    /// already prepared before this background SQLite query started.
    ResponseDraftReady {
        kind: ResponseKind,
        /// The mailbox the letter being answered lives in -- the owner of
        /// the draft Core just created. Every compose door has to act on
        /// this account, not on whichever one the account menu shows.
        account_id: AccountId,
        source_message_id: MessageId,
        result: Result<Draft, String>,
    },
    DiscardDraft,
    PickAttachment,
    AttachmentPicked(PathBuf),
    /// `session`: see [`Msg::DraftSaved`]. This arm rebinds
    /// `compose_draft_id`, so a late answer must not reach a newer window.
    AttachmentAdded {
        session: u64,
        result: Result<(DraftId, Vec<DraftAttachment>), String>,
    },
    /// T-046: include or remove one already cached incoming attachment from
    /// the current Forward draft through Core's draft-attachment command.
    ForwardAttachmentToggled {
        attachment: Attachment,
        selected: bool,
    },
    RemoveAttachment(String),
    /// T-043: open an incoming attachment with the system handler. The
    /// attachment stays metadata-only on the GTK channel; data downloads
    /// through `SyncHandle` and is opened from its disk cache.
    OpenAttachment(Attachment),
    /// T-043: choose a destination for an incoming attachment. Carries a
    /// clone so the chooser can finish after the reading selection changed.
    SaveAttachmentAs(Attachment),
    AttachmentSavePicked {
        attachment: Attachment,
        destination: PathBuf,
    },
    AttachmentOpened {
        attachment_id: AttachmentId,
        ok: bool,
    },
    AttachmentSaved {
        attachment_id: AttachmentId,
        ok: bool,
    },
    Send,
    /// `session`: see [`Msg::DraftSaved`]. This arm rebinds
    /// `compose_draft_id` and hides the compose window, so a late answer
    /// must not close a window the reader has only just opened.
    SendQueued {
        session: u64,
        result: Result<(DraftId, OperationId), String>,
    },
    SearchChanged(String),
    SearchDebounced {
        gen: u64,
        query: String,
    },
    /// T-049, D11: completion of the background `Core::search` call
    /// kicked off by `App::spawn_search` -- `gen` is compared against
    /// `App::search_gen`, the exact same guard shape as `ThreadsLoaded`/
    /// `BodyLookup` above, so a reply for a query the user has since
    /// typed past (or a popover already closed and reopened on a new
    /// query) is dropped instead of rendered -- see
    /// `search_stale_generation_reply_is_dropped_not_the_newer_one`.
    /// `append` is `false` for a fresh query's first page (results
    /// replace whatever the popover currently shows) and `true` for a
    /// "load more" page (`Msg::SearchLoadMore`), whose results are
    /// appended onto what is already shown instead of replacing it.
    /// `result` is `Core::search`'s own `Result<SearchResults,
    /// CoreError>` with the error reduced to its already-non-secret
    /// `message` field, mirroring `BodyLookup`'s `result` above.
    SearchResults {
        gen: u64,
        append: bool,
        result: Result<feathermail_core::SearchResults, String>,
    },
    /// T-049 (г): "load more" -- fetch the next page for the query
    /// already in `App::search_draft`, using `App::search_next` as the
    /// cursor. A no-op if there is no next page (handled in `update`, not
    /// here, so this message itself never needs to encode "nothing to
    /// load").
    SearchLoadMore,
    /// T-049 (д): the user clicked a recent-query suggestion in the
    /// popover's empty-draft state. Carries the picked query text, which
    /// `update` feeds into `search_entry.set_text`, piggy-backing on that
    /// entry's existing `connect_changed` -> `Msg::SearchChanged` wiring
    /// instead of duplicating the debounce/parse/dispatch pipeline for a
    /// second entry point.
    SearchHistoryPick(String),
    OpenSearch,
    CloseSearch,
    SearchDismissed,
    OpenSearchHit(String),
    /// T-090: the root window's `notify::is-active` (wired in
    /// `App::init`'s view). Carries the new value; feeds
    /// `PowerState::app_backgrounded` via `App::report_viewport` -- the
    /// app itself is the only possible source of "is this app's window
    /// focused".
    WindowActiveChanged(bool),
    SetFilter(ThreadFilter),
    Next,
    Prev,
    Open,
    FocusSearch,
    Reply,
    /// T-032: opens a Core-created Reply All draft for the expanded letter.
    ReplyAll,
    /// T-029/T-046: opens a Core-created forward draft for the expanded
    /// letter, without reply threading headers.
    Forward,
    /// T-032 (D39): hover-strip actions carry the row's own thread id, so
    /// they act on a thread the user did **not** select -- they must not
    /// go through `select_thread` / T-028's mark-read-on-select policy
    /// and must not load a body. Each is the same `Core::dispatch`
    /// command the toolbar sends, aimed at the row's id.
    RowArchive(String),
    RowDelete(String),
    RowMarkRead(String),
    RowSnooze(String),
    /// T-054: the right button on a message row. Carries the row's own
    /// thread id and the click point in the `GtkListView`'s coordinates,
    /// because the menu that opens is the shell's -- parented to the list,
    /// not to the row, which `rebind_thread` disposes the moment the
    /// selection this message asks for goes through.
    RowContextMenu {
        id: String,
        x: f64,
        y: f64,
    },
    /// T-032: the single door every Delete entry (`Msg::Delete`, hover
    /// `RowDelete`, context menu) ends at. Emitted either directly (when
    /// `prefs.confirm_delete` is off) or by the confirm dialog's Delete
    /// button; the arm dispatches `Command::Trash` for these ids.
    DeleteConfirmed {
        thread_ids: Vec<String>,
    },
    /// D28: explicit irreversible deletion, separate from `Delete`/Trash.
    /// It uses the same confirmation preference (default ON) and then
    /// dispatches `Command::PermanentDelete` through Core.
    PermanentDelete,
    PermanentDeleteConfirmed {
        thread_ids: Vec<String>,
    },
    /// T-029: accordion — expand this message of the already-open thread.
    /// Collapsed cards emit this; the reading pane still has one
    /// `IsolatedHtmlView` and loads a body only for the expanded id.
    ExpandThreadMessage(MessageId),
    Escape,
    OpenSettings,
    CloseSettings,
    SetSettingsPage(SettingsPage),
    OpenWelcome,
    OpenAddAccount,
    ShowOtherForm,
    UseMailboxPreset(MailboxPreset),
    AddMailbox,
    /// T-094: completion of the one-shot provisioning thread
    /// (`feathermail_service::spawn_provision`) kicked off by
    /// `Msg::AddMailbox` -- the same `gen`-guarded shape
    /// as `ThreadsLoaded`/`BodyLookup`, so a reply for a wizard the user
    /// has since backed out of is dropped instead of applied. `result` is
    /// the new account id or human-only error text (D14: passwords,
    /// tokens and protocol details never cross this channel).
    Provisioned {
        gen: u64,
        result: Result<String, String>,
    },
    WizardSecurityChanged,
    /// T-156: focus left the wizard's address field. Not `changed`: every
    /// one of these may cost an HTTPS request plus two DNS queries, so the
    /// lookup is started once the address is finished rather than once per
    /// keystroke. The arm decides whether it is worth asking at all
    /// (`autoconfig_trigger`); this message only says the moment arrived.
    WizardEmailEntered,
    /// T-156: the one-shot autoconfig thread
    /// (`feathermail_service::spawn_autoconfig`) started by
    /// `Msg::WizardEmailEntered` has answered. `email` is the address the
    /// lookup was started for -- there is no way to cancel the thread, so a
    /// reply is only applied while the field still holds that address, the
    /// same guard `Msg::Provisioned` gets from `gen`. D14: neither half of
    /// this message prints the address.
    AutoconfigResolved {
        email: WizardEmail,
        outcome: AutoconfigOutcome,
    },
    WizardBack,
    OpenInboxFromWizard,
    EditAccount(String),
    CancelEditAccount,
    SaveAccountName {
        id: String,
        name: String,
    },
    RemoveAccountRequest(String),
    RemoveAccountCancel,
    RemoveAccountConfirm(String),
    SwitchAccount(String),
    /// T-108: the account menu asked for the merged view -- every mailbox's
    /// Inbox, Sent, Starred and Trash in one list. Carries nothing: the
    /// merged view has no account to name, and which of its four folders is
    /// open is ordinary `SelectFolder` state afterwards.
    OpenUnified,
    OpenNotification {
        account_id: String,
        thread_id: String,
    },
    ToggleAccountNotifications(String),
    Diagnostics(&'static str),
    ExportDiagnosticsPicked(PathBuf),
    DiagnosticsDone(Result<String, String>),
    CheckUpdates,
    UpdateChecked {
        result: UpdateCheck,
        manual: bool,
    },
    OpenUpdate,
    Refresh,
    PrefetchOlder,
    PrefetchNewer,
    CancelFolder,
    CreateFolder,
    /// T-097(11): the New-folder popover just opened -- clear the name and
    /// preselect the colour swatch Core would have assigned anyway, so the
    /// picker starts on the answer the user gets by ignoring it.
    BeginCreateFolder,
    /// T-060t: the Rename popover just opened -- prefill the entry with the
    /// folder's current name so the common case (fix a typo) is one edit,
    /// not a retype.
    BeginRenameFolder,
    CancelRenameFolder,
    /// T-060t: rename the folder currently open, through
    /// `Core::rename_folder`. Never an Undo step: the inverse is another
    /// rename, which the user can perform the same way.
    RenameFolder,
    /// T-060u: the user asked to delete the folder currently open. Opens the
    /// confirmation; deleting a mailbox is irreversible on the server and
    /// outside the Undo history, so nothing happens on this message alone.
    DeleteFolder,
    /// T-060u: the confirmation came back OK. The only door to
    /// `Core::delete_folder` from the GTK side.
    DeleteFolderConfirmed(String),
    /// T-097(10): the sidebar row's own menu asked to rename that folder.
    /// The toolbar pair only ever acted on the folder already open, which
    /// is why deleting one was hard to find -- you had to open it first.
    /// Carries the id because the row menu can name a folder the user is
    /// not looking at.
    RenameFolderFrom(String),
    /// T-097(10): the rename popover is anchored to the toolbar button, so
    /// it can only be opened once the folder is selected and that button is
    /// on screen. Sent after `RenameFolderFrom`'s `SelectFolder` so the view
    /// pass in between has made it visible.
    OpenRenameFolder,
    /// T-097(10): the sidebar row's own menu asked to delete that folder.
    /// Opens the same confirmation `DeleteFolder` does -- nothing reaches
    /// Core on this message alone.
    DeleteFolderFrom(String),
    /// T-106: the sidebar row's own menu asked to mark that whole folder
    /// read. Carries the folder id rather than acting on the open folder:
    /// the menu can be opened on a row the user has not selected, and
    /// right-clicking a row deliberately does not select it.
    MarkFolderRead(String),
    /// T-074: completion of the background `Core::list_threads` fetch
    /// (D11 -- must not run on the GTK thread for a large mailbox).
    /// `gen` is compared against `App::threads_gen` so a reply for a
    /// folder/account the user already left is dropped.
    /// T-125: `first` is true for a page requested with no cursor -- the
    /// page that *is* the section, as opposed to an older page scrolled
    /// into view, which appends. The arm cannot infer this from `gen`
    /// (both carry the same one) and must not guess: guessing wrong
    /// either wipes the list on a scroll or duplicates it on a switch.
    ThreadsLoaded {
        gen: u64,
        first: bool,
        page: ThreadPage,
    },
    /// T-129: the warm-up list for the folder on screen, read off the GTK
    /// thread. `gen` is `App::threads_gen`, so a list for a folder the
    /// reader has already left is dropped rather than queued.
    WarmupNeeded {
        gen: u64,
        needed: Vec<(AccountId, MessageId)>,
    },
    /// T-128: a folder's sync pass has been over for `SYNC_STRIP_LINGER`
    /// and no other pass picked up where it left off, so the fetching
    /// strip may come down. `gen` is checked against the entry in
    /// `App::settling_folders`: a pass that restarted in the meantime has
    /// a newer one, and this arrives as a no-op.
    SyncFolderSettled {
        account_id: String,
        folder_id: String,
        gen: u64,
    },
    /// T-132: ask how far the first-run backfill has got. Sent when a
    /// pass starts and re-armed by the reply while anything is still
    /// backfilling; the query itself runs on the reader handle, off the
    /// GTK thread.
    PollSyncProgress,
    /// T-132: the answer -- `None` when nothing is backfilling, which is
    /// also what stops the polling.
    SyncProgressLoaded(Option<SyncProgress>),
    /// T-125: completion of the background `Core::list_folders` that
    /// switching to another mailbox now kicks off instead of running on
    /// the click. Carries the folder set the new account opens on, so the
    /// arm can pick its Inbox, paint the sidebar and start the list --
    /// none of which the click itself is allowed to wait for. `gen` is
    /// `App::focus_gen`, the same stale-reply guard as `ThreadsLoaded`:
    /// a reply for a mailbox the user has already switched away from is
    /// dropped, not applied.
    AccountFocused {
        gen: u64,
        folders: Vec<FolderSummary>,
    },
    ThreadsLoadFailed {
        gen: u64,
        /// T-125: as in `ThreadsLoaded` -- a failed *append* has no
        /// business emptying rows that did load.
        first: bool,
        message: String,
    },
    /// T-121: sidebar counts (`list_folders` / `list_unified_folders`)
    /// finished off the GTK thread. Same `gen` guard as `ThreadsLoaded`:
    /// a recount for an account the user already left is dropped.
    NavLoaded {
        gen: u64,
        folders: Vec<FolderSummary>,
        /// T-161: the folders a row chip may name, which is not the same
        /// list the sidebar shows. In the merged view the sidebar is the
        /// four unified folders and a row's own folder belongs to one
        /// mailbox, so the chip has to look the label up somewhere that
        /// knows every account's folders. In a single mailbox the two
        /// lists are the same and this carries a copy of it, built on the
        /// same background thread rather than cloned on the GTK one.
        chip_folders: Vec<FolderSummary>,
    },
    /// T-080: completion of the background `Core::lookup_body` disk read
    /// kicked off by `App::request_body`/`App::lookup_body_async` -- the
    /// exact same `gen`-guarded shape as `ThreadsLoaded` above, so a reply
    /// for a message the user has since navigated away from is dropped
    /// instead of overwriting whatever is now selected. `message_id` is
    /// carried along only for a `debug_assert_eq!` inside
    /// `App::apply_body_lookup` that the generation guard and the id
    /// never actually disagree; see that method's doc comment. `result`
    /// is `Core::lookup_body`'s own `Result<BodyLookup, CoreError>` with
    /// the error reduced to its already-non-secret `message` field,
    /// mirroring `ThreadsLoadFailed`'s `err.message` above.
    /// T-109: the backoff timer armed after a failed live body fetch has
    /// fired. `gen` is the usual `body_gen` guard -- if the reader has
    /// moved to another message since, this is stale and does nothing.
    RetryBody {
        gen: u64,
    },
    /// T-109: the reader pressed "Try again" in the reading pane. Unlike
    /// [`Self::RetryBody`] this carries no generation (the button lives on
    /// a `#[watch]` and would go stale) and restarts the backoff ladder:
    /// a person asking is new information.
    RetryBodyNow,
    BodyLookup {
        gen: u64,
        message_id: MessageId,
        result: Result<BodyLookup, String>,
    },
    /// T-030, D11: completion of the background parse+sanitize kicked
    /// off after a cache hit. Same `gen` guard as [`Self::BodyLookup`].
    /// `prepared` has already been through `feathermail_html::sanitize`
    /// when it is HTML — this is the only message that is allowed to
    /// carry HTML toward the WebView.
    BodyPrepared {
        gen: u64,
        prepared: PreparedBody,
    },
    /// T-030: per-message override for the "Block remote images" setting.
    /// Re-runs prepare with `allow_remote_images = true` for the message
    /// currently in the reading pane. T-117: also remembers the sender
    /// domain so later letters from the same host load images.
    ShowRemoteImages,
    /// T-117: forget the current sender domain and this message's
    /// session override, then re-run prepare with images blocked.
    HideRemoteImages,
    /// T-030: a link inside the HTML body was activated. The WebView
    /// has already refused to navigate; this is the request to open
    /// (or confirm, or refuse) in the external browser.
    HtmlLinkActivated(String),
    /// T-030: the user confirmed the "open this link" dialog.
    HtmlLinkOpenConfirmed(String),
    /// T-078 (c): an event from the background sync worker
    /// (`feathermail_service::SyncHandle`'s `events` callback), re-emitted
    /// through the input sender so it runs on the GTK thread instead of
    /// the worker thread that produced it (D11 -- see
    /// `App::handle_sync_event`'s doc comment).
    Sync(feathermail_service::SyncEvent),
    /// T-067/D11: the root GTK window has mapped and yielded its first idle
    /// turn. This is the only activation path for the deferred sync worker,
    /// so no IMAP connection can race ahead of the first visible shell.
    ActivateSyncAfterFirstFrame,
    /// T-067: the idle turn after the first frame has come round, so the
    /// rows the first paint held back can go into a `gtk::ListView` that
    /// now knows its own size. `gen` is the `App::threads_gen` the page was
    /// fetched under, checked the same way `ThreadsLoaded` checks it.
    RenderFirstPaintTail {
        gen: u64,
    },
    /// T-078 (c), D11 fix (review round 2): fires once per coalesced
    /// burst of `Acked`/`Failed` events, `SYNC_REFRESH_COALESCE_MS` after
    /// the first one -- see `App::request_sync_refresh`'s doc comment.
    SyncRefreshDue,
    /// T-028, D27: the Delay (2s) mark-read timer fired. Applied only if
    /// `gen` still matches `App::mark_read_gen` and `thread_id` is still
    /// the selected thread on the Inbox -- leaving the thread (or the
    /// Inbox) bumps the generation and this message becomes a no-op. See
    /// `delayed_mark_read_is_current`.
    MarkReadDelayFired {
        gen: u64,
        thread_id: String,
    },
    /// T-139: a mark-read/mark-unread the mailbox writer thread has
    /// finished. The GTK thread painted the rows the moment the reader
    /// asked; this is the database's answer coming back -- the rows are
    /// re-read from Core, the sidebar recounted, the Undo toast (if this
    /// was an explicit mark, `announce`) raised, and `error` shown rather
    /// than dropped, which is the defect itself: a mark that lost the race
    /// against the backfill used to fail silently.
    MarkWritten {
        ids: Vec<String>,
        read: bool,
        announce: bool,
        tickets: Vec<UndoTicket>,
        error: Option<String>,
    },
    /// T-143: an Archive queued by the mailbox writer has reached Core.
    /// Rows disappear optimistically on click; a refused write reloads the
    /// list from Core, while a successful receipt supplies both Undo and the
    /// operation ids whose later provider failure restores the rows.
    ArchiveWritten {
        tickets: Vec<UndoTicket>,
        operation_ids: Vec<OperationId>,
        error: Option<String>,
    },
    /// T-143: a local Snooze queued by the mailbox writer has reached Core.
    /// `label` is one of the fixed UI strings chosen by the click path; no
    /// message content or user input crosses this boundary.
    SnoozeWritten {
        ids: Vec<String>,
        label: &'static str,
        tickets: Vec<UndoTicket>,
        error: Option<String>,
    },
    /// A Star/Unstar the mailbox writer has finished. `starred` is the
    /// *pre*-toggle flag the click read, so it says which command was sent
    /// -- and a starred-only list may drop the row only here, once Core has
    /// actually taken the star off. Deciding that on the click hid a thread
    /// the database had refused to unstar, and only a folder switch brought
    /// it back.
    StarWritten {
        ids: Vec<String>,
        starred: bool,
        error: Option<String>,
    },
    /// A Trash the mailbox writer has finished. The rows left the list on
    /// the click; this is Core agreeing -- with the durable Undo tickets --
    /// or refusing, in which case the list is re-read.
    DeleteWritten {
        label: &'static str,
        tickets: Vec<UndoTicket>,
        error: Option<String>,
    },
    /// A permanent delete the mailbox writer has finished. Deliberately
    /// ticketless: T-034 keeps permanent deletion outside Undo, so the
    /// receipt's tickets are dropped on the writer thread rather than
    /// carried here where a toast could offer them.
    PermanentDeleteWritten {
        label: &'static str,
        error: Option<String>,
    },
    /// An Unsnooze the mailbox writer has finished. `woken` counts the
    /// threads that actually had a deadline; threads that were not snoozed
    /// are skipped, not failed.
    UnsnoozeWritten {
        woken: usize,
        failed: bool,
    },
    /// The Undo the mailbox writer has finished. The tickets were consumed
    /// on the GTK thread the moment the button was pressed, so a second
    /// press cannot undo the same operation twice while this is in flight.
    UndoWritten {
        failed: bool,
    },
    /// A draft the mailbox writer has deleted -- from Discard, or from the
    /// discard that was parked on an in-flight save. The window is already
    /// closed; only a refusal has anything left to say.
    DraftDiscarded {
        error: Option<String>,
    },
    /// An attachment the mailbox writer has removed from the open draft.
    /// `session` guards the same race `AttachmentAdded` does: by the time
    /// this lands the reader may be writing the next letter.
    AttachmentRemoved {
        session: u64,
        id: String,
        error: Option<String>,
    },
    /// T-049 (д): the recent-query list, read off the GTK thread on the
    /// same reader handle `refill_nav` uses.
    SearchHistoryLoaded(Vec<String>),
    /// T-163: `Core::create_folder_with_color` finished on the mailbox
    /// writer thread.
    ///
    /// `gen` is [`App::reader_place`] as it stood when the button was
    /// pressed -- which mailbox, which folder, which settings editor. The
    /// answer moves the reader into the new folder only while that is
    /// still true; a reader who has navigated away in the meantime, or who
    /// is halfway through typing the next folder's name, keeps what they
    /// have. `result` is the new folder's id, or Core's own error text
    /// shown verbatim (D14: no protocol detail, no credentials).
    FolderCreated {
        gen: u64,
        result: Result<String, String>,
    },
    /// T-163: `Core::rename_folder` finished on the mailbox writer. `Ok`
    /// carries whether the RENAME was queued for the server (a folder the
    /// server has never heard of is renamed locally and queues nothing).
    FolderRenamed {
        gen: u64,
        result: Result<bool, String>,
    },
    /// T-163: `Core::delete_folder` finished on the mailbox writer. Same
    /// `Ok(queued)` as [`Msg::FolderRenamed`].
    ///
    /// Deliberately without a `gen`, unlike its two siblings: everything
    /// this answer does -- drop the section cache, wake the worker,
    /// recount the sidebar -- is true wherever the reader has got to,
    /// because the folder is gone for all of them. The one thing that
    /// would need a stamp, moving a reader off the folder they were
    /// standing in, is not done here at all: it lands in `Msg::NavLoaded`,
    /// under that message's own generation guard, once `list_folders` has
    /// actually stopped returning it.
    FolderDeleted {
        result: Result<bool, String>,
    },
    /// T-163: `Core::update_account` finished on the mailbox writer. The
    /// account id travels with the answer so the settings editor is closed
    /// only if it is still the editor for that account.
    AccountNameSaved {
        gen: u64,
        id: String,
        error: Option<String>,
    },
    /// T-163: `Core::remove_account` finished on the mailbox writer --
    /// together with the `SecretStore::connect()` D-Bus round trip that
    /// used to happen on the GTK thread in front of it. `Ok` carries the
    /// report's `keyring_error`: local removal committed either way, so
    /// the account is gone from the UI and this is only a heads-up that a
    /// stray keyring entry may remain.
    AccountRemoved {
        gen: u64,
        account_id: AccountId,
        result: Result<Option<String>, String>,
    },
    /// T-163: a Move the mailbox writer has finished. Same shape as
    /// [`Msg::DeleteWritten`]: the rows are re-read from Core either way,
    /// and a successful receipt supplies the Undo tickets.
    MoveWritten {
        tickets: Vec<UndoTicket>,
        error: Option<String>,
    },
    /// T-140: "Mark all as read" on a folder -- one mailbox's or every
    /// mailbox's -- has finished on the mailbox writer thread.
    FolderMarkedRead {
        folder_id: String,
        /// The mailbox the click was aimed at, or `None` for the merged
        /// item -- which of the two decides how far the withdrawn
        /// notifications reach (T-124), and it is carried rather than
        /// re-derived because the reader may have switched mailbox while
        /// the write was running.
        account_id: Option<AccountId>,
        tickets: Vec<UndoTicket>,
        error: Option<String>,
    },
}

/// Local-only snooze choices (T-035/D26).  Snooze never becomes an IMAP
/// command; the Core scheduler wakes it back into Inbox at the deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnoozePreset {
    OneHour,
    LaterToday,
    Tomorrow,
    NextMonday,
}

/// A first-class manual-connection preset. It only fills the ordinary IMAP/
/// SMTP form; it never starts OAuth or sends the user to a browser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailboxPreset {
    Google,
    Microsoft,
    Yandex,
}
