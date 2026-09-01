//! The background sync worker (T-078, first half): the thing that
//! actually calls [`feathermail_core::Core::tick`] so a queued operation
//! reaches the server on its own, instead of only ever being driven by a
//! test (see this crate's own top-level doc comment and `plan.md`'s
//! T-078 entry for why that was the state of the world before this file
//! existed).
//!
//! # D11 -- never call this from a GTK callback
//!
//! Everything in this module runs on the one background thread [`start`]
//! spawns (D12: a thread, not a separate process -- that split is Phase 2,
//! T-109). The GTK shell must never call [`Core::tick`], `ProviderFactory`,
//! or anything else in this crate directly from a callback on the UI
//! thread; the only contact points are [`SyncHandle::wake`] /
//! [`SyncHandle::shutdown`] (commands going in) and the `events` callback
//! [`start`] was given (events coming out). Both are safe to call from any
//! thread -- `wake`/`shutdown` just push onto an `mpsc::Sender`, and the
//! events callback runs on *this* worker thread, so the caller supplied at
//! `start` time is responsible for hopping back to the GTK main loop
//! itself (e.g. `glib::idle_add` / a `relm4` sender) exactly the way any
//! other cross-thread event already has to.
//!
//! # D14 -- what an event may say
//!
//! [`SyncEvent`] never carries a password, an OAuth token, or message
//! content. [`SyncEvent::ConnectFailed`] carries only a
//! [`feathermail_core::ConnectError`]'s human `message` (never its
//! `details`, which can echo raw protocol text -- see
//! `connect_error_message`'s doc comment); [`SyncEvent::Failed`] carries
//! [`feathermail_core::ApplyError`], which is a bare tag
//! (`Network`/`Auth`/`Conflict`/`NotFound`/`Unsupported`) with no payload
//! at all.
//!
//! # One live session, one account
//!
//! A live IMAP session ([`feathermail_providers::ImapMailProvider`]) is
//! authenticated as exactly one mailbox, so the operation it is handed
//! must belong to that same mailbox. [`Core::tick`] cannot promise that:
//! it claims the oldest due operation across *every* account, and would
//! happily run account B's `UID MOVE`/`UID STORE` down account A's
//! socket, against whatever UIDs happen to sit at those numbers in A's
//! folder. Since T-081 that is not even a loud failure -- the apply
//! fails, the failure is non-retryable, and the queue rolls the user's
//! local mark back.
//!
//! So this worker never calls `tick`. It picks one account, connects a
//! provider for it, and drains [`Core::tick_for_account`] against that
//! account and no other.
//!
//! The flip side is that `Idle` from `tick_for_account` means "nothing
//! due for *this* account", which is not "nothing due at all". The loop
//! below therefore treats a per-account `Idle` as the end of that
//! account's turn -- hand the turn to the next account -- and only
//! actually sleeps once every account in turn has come back idle. Reading
//! the first `Idle` as a reason to sleep would let one quiet account park
//! the whole worker for [`MAX_IDLE_POLL_SECS`] while another account's
//! archive sat in the queue.
//!
//! # T-078 (b) -- inbound sync, and what is still not wired
//!
//! An empty operation queue (`TickOutcome::Idle`) says nothing about
//! whether new mail is waiting on the server -- that never shows up as a
//! queued [`feathermail_core::Operation`]. So on every per-account `Idle`,
//! [`sync_one_due_folder`] separately asks [`Core::folder_sync_inputs`] /
//! [`next_sync`] whether one of this account's folders is due for an
//! inbound pass, and if so, runs exactly one [`feathermail_sync::sync_folder`]
//! call against it -- never a whole account's folders in one turn, for the
//! identical no-starvation reason `tick_for_account` only ever drains one
//! account's queue at a time (see above).
//!
//! # T-090 -- `Focus` and `PowerState` have real sources now
//!
//! Two of `next_sync`'s inputs stopped being permanent defaults:
//! - **`Focus.open_folder`** -- the GTK shell reports the folder it is
//!   displaying through [`SyncHandle::report_viewport`], which writes a
//!   shared [`Viewport`] cell this loop snapshots before every
//!   [`sync_one_due_folder`] call. A shared cell, deliberately **not** a
//!   new `WorkerCommand`: every blocking path in this module
//!   (`wait_for_shutdown`, `sync_one_due_folder`'s own `is_cancelled`
//!   drain, `run_idle_with`'s `should_stop`, `run`'s top-of-loop drain)
//!   reads from `rx` independently, so a channel-carried focus update
//!   could be consumed by whichever drain ran first and never reach the
//!   scheduler -- the same lost-command hazard T-080/T-089 already had
//!   to route around for `FetchBody`. A cell with exactly one writer
//!   (the handle) and a fresh read per decision cannot be lost that way;
//!   the `Wake` `report_viewport` also sends only needs to make a parked
//!   worker re-evaluate promptly, and `Wake` is already a safe no-op at
//!   every one of those drains.
//! - **`PowerState.on_battery` / `app_backgrounded`** -- `on_battery`
//!   comes from [`PowerProbe`] ([`SysfsPowerProbe`] in production, fixed
//!   fakes in tests), read fresh at the same snapshot point;
//!   `app_backgrounded` rides the same [`Viewport`] report, because the
//!   only possible source of "is this app's window focused" is the app
//!   itself. `screen_locked_or_idle` and `network_metered` still pass
//!   `false` always -- their only real sources are session D-Bus
//!   (logind/UPower) and NetworkManager respectively, and this crate
//!   links neither today; see plan.md T-090 for that remainder.
//!
//! `Focus.open_thread_folder` stays `None` on purpose: the shell today
//! only ever displays an open thread inside the folder it is also
//! showing (`OpenSearchHit` switches the displayed folder to the hit's
//! own first), so there is no second folder signal to report.
//!
//! # T-089 -- IMAP `IDLE` on the folder in view, one account at a time
//!
//! [`watch_inbox_for_push`] is called instead of a plain timed sleep
//! whenever this account is otherwise idle (no queued operation due, no
//! queue retry backoff pending) and [`idle_watch_folder`] can name a
//! mailbox to `SELECT`. It holds this account's live session in IMAP
//! `IDLE` (or the honest `NOOP` fallback, D30) on that one folder, so a
//! server push is noticed immediately rather than up to
//! [`feathermail_sync::schedule::INBOX_INTERVAL_SECS`] late, and syncs
//! it right away when one arrives.
//!
//! Which folder: [`idle_watch_folder`] prefers the shell's
//! [`Viewport::open_folder`] when that id belongs to *this* account
//! (T-090's cell, snapshotted on this same pass), and the account's
//! Inbox otherwise -- Settings, another account's folder, and overlay
//! ids (`starred`/`snoozed`) all miss and fall through. A
//! [`SyncHandle::report_viewport`] writes the cell and sends `Wake`,
//! which already breaks an in-flight `IDLE` round; the next pass
//! `SELECT`s the new mailbox. That is the SELECT-hop T-089's artifact
//! asked for, not a second command. One socket can only idle one
//! mailbox, so Inbox is not watched at the same time as the open
//! folder -- D30's "Inbox + текущая папка" is a scheduler priority,
//! not two concurrent `IDLE`s.
//!
//! More than one account: still one live session, still this one
//! thread. The slice is capped at [`MULTI_ACCOUNT_IDLE_SECS`] and the
//! session is dropped afterwards so the round-robin can hand the
//! socket to the next account. Simultaneous `IDLE` on every account
//! is out of scope per the ticket ("`IDLE` сразу на всех папках всех
//! аккаунтов -- это отдельный разговор").
//!
//! A pending `TickOutcome::Retry` backoff (`known_delay`/`retry_delay`
//! below) still keeps the old plain timed wait -- paying `IDLE`'s
//! `SELECT`+`CAPABILITY`+`IDLE`/`DONE` round trips for what is often a
//! two-second D32 floor is not worth it, and that precise timer stays
//! on the wait primitive that already gets it exactly right
//! (`retry_backs_off_and_does_not_hot_loop`).
//!
//! # T-092 -- `fts_pending` had no drain call site anywhere
//!
//! T-048 built [`feathermail_core::Core::index_pending_batch`] (bounded,
//! one SQL transaction per call) but nothing in the workspace ever called
//! it -- `messages_fts` never filled in, `SearchResults::pending_index`
//! only grew, and search quietly degraded to "finds nothing" as more mail
//! synced. [`drain_one_index_batch`] is the call site, invoked once near
//! the very top of `run`'s loop, unconditionally -- see that function's
//! own doc comment for why unconditionally (not gated on this account's
//! queue/folder being idle, unlike [`sync_one_due_folder`]) and for
//! requirement 3's decision on what an `Err` from it does.
//!
//! No new [`SyncEvent`] variant carries this. `SearchResults::pending_index`
//! (`crates/core/src/search.rs`) already comes back from *every*
//! [`feathermail_core::Core::search`] call, and `crates/app/src/shell.rs`
//! already reads it from every one of those responses
//! (`search_pending_index`, T-049) to decide whether to show "still
//! indexing N message(s)". A worker event here would only ever carry a
//! count -- D14 forbids anything else -- and that count would still just
//! restate what the next `search` call already reports; the UI has no
//! push subscription to indexing progress today (searching is user-
//! triggered, not live-updated), so a duplicate number pushed from a
//! second, worker-owned code path is not a gap being closed, it is a
//! second source of truth for the same fact. Deliberately not added.
//!
//! # Why `crates/app` cannot drive `index_pending_batch` itself
//!
//! Same reasoning as the rest of this file's D11 section above: draining
//! `fts_pending` means at least one SQLite write transaction plus, per
//! row, one read of a cached body file off disk (see
//! `crates/core/src/search.rs`'s `index_one`/`body_text_for_index`) --
//! disk I/O with no bound the GTK thread could rely on, exactly the class
//! of work D11 exists to keep off it. `crates/service/tests/
//! app_never_calls_index_pending_batch.rs` is this ticket's fail-closed
//! check that `crates/app`'s source never spells out the one symbol that
//! does this work -- see that file's own doc comment for exactly what it
//! does and does not prove.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use feathermail_core::body::{
    default_attachments_dir, default_bodies_dir, DEFAULT_SNIPPET_REPAIR_BATCH, PREFETCH_BODIES,
    PREFETCH_CHUNK,
};
use feathermail_core::{
    AccountId, ApplyError, AttachmentId, ConnectError, Core, CoreError, FolderId, IndexBatchResult,
    MessageId, OperationId, RemoteLocator, ThreadId, TickOutcome, DEFAULT_INDEX_BATCH,
};
use feathermail_providers::{IdleOutcome, IDLE_TIMEOUT_SECS, NO_IDLE_POLL_SECS};
use feathermail_sync::schedule::{next_sync, Decision, Focus, FolderInput, FolderRole, PowerState};
use feathermail_sync::SyncError;

use crate::provider_factory::{IdleRound, MailSession, ProviderFactory};

/// Upper bound on how long the worker sleeps while it believes there is
/// nothing to do (D12: a bounded poll, not an unbounded block, so an
/// operation whose `next_attempt_at` was scheduled by a *previous* run of
/// this process -- which this fresh worker has no in-memory record of --
/// is still eventually revisited even though nothing woke it early). Tied
/// to D32's own retry ceiling (`feathermail_sync::backoff`'s 15-minute
/// cap): there is never a legitimate reason for this worker to want a
/// longer bound than the longest delay the retry table itself ever hands
/// out.
const MAX_IDLE_POLL_SECS: i64 = 15 * 60;

/// How long one account may hold IMAP `IDLE` when at least one other
/// account is also connected (T-089 remainder). One worker thread, one
/// live session: parking that session on account A's mailbox for a full
/// RFC 2177 ceiling (up to 29 minutes) would starve B's queue and B's
/// own push-watch until A happened to time out. A short slice, then
/// drop the session and round-robin, is the honest bound this ticket
/// can keep without opening a second connection (explicitly out of
/// scope: "`IDLE` сразу на всех папках всех аккаунтов"). 30s is under
/// [`feathermail_sync::schedule::INBOX_INTERVAL_SECS`] (45), so even
/// the unfocused account still hears a push sooner than the pre-T-089
/// poll, and a `Wake`/`FetchBody` still cuts the slice short the same
/// way a single-account round does.
const MULTI_ACCOUNT_IDLE_SECS: i64 = 30;

/// Cooldown after a failed [`Core::index_pending_batch`] call, in seconds
/// (T-092, requirement 3). Local SQLite/disk trouble is not the kind of
/// transient [`feathermail_sync::backoff`]'s D32 table exists for (that
/// grows 2s/5s/15s/... on purpose to spare a flaky *network* peer) -- a
/// fixed, modest pause is enough to turn "one broken row spins this loop
/// at full CPU" into "retried at a sane, bounded rate", without
/// pretending a growing delay makes a wedged local database more likely
/// to recover. See `a_failing_index_batch_backs_off_and_never_hot_loops`.
const INDEX_ERROR_COOLDOWN_SECS: i64 = 30;

/// Seam over [`Core::index_pending_batch`] so [`drain_one_index_batch`]'s
/// error-cooldown behavior (T-092, requirement 3) can be proven with a
/// fake that fails on command -- the same reason [`WorkerClock`] exists
/// as a trait rather than `run` just calling `SystemTime`/`recv_timeout`
/// directly: a real SQLite failure on a healthy, freshly created profile
/// is not something a test can trigger to order without corrupting the
/// very file the rest of the worker's test also needs to keep working.
/// Production has exactly one implementation, [`Core`] itself, calling
/// straight through to its own inherent method of the same name.
trait IndexBatcher {
    fn index_pending_batch(
        &self,
        bodies_dir: &Path,
        limit: usize,
    ) -> Result<IndexBatchResult, CoreError>;
}

impl IndexBatcher for Core {
    fn index_pending_batch(
        &self,
        bodies_dir: &Path,
        limit: usize,
    ) -> Result<IndexBatchResult, CoreError> {
        Core::index_pending_batch(self, bodies_dir, limit)
    }
}

/// One [`DEFAULT_SNIPPET_REPAIR_BATCH`]-sized pass over `snippet_repairs`
/// (T-134). Same shape, same loop turn and the same reason as
/// [`drain_one_index_batch`] below: recomputing a preview means reading a
/// cached body off disk, which the GTK thread may not do (D11), and it is
/// local work with no `account_id`, so it must keep making progress while
/// every account sits in connect backoff.
///
/// The queue is finite and seeded once by the v26 migration: it drains and
/// then costs one `COUNT(*)`-free `SELECT ... LIMIT` per loop turn forever
/// after, exactly like `fts_pending` when the index is caught up. Errors
/// share [`INDEX_ERROR_COOLDOWN_SECS`]' reasoning but keep their own
/// deadline, so a wedged repair cannot silently stall indexing.
fn drain_one_snippet_repair_batch(
    core: &Core,
    bodies_dir: &Path,
    now: i64,
    cooldown_until: &mut i64,
) -> bool {
    if now < *cooldown_until {
        return false;
    }
    match core.repair_snippet_batch(bodies_dir, DEFAULT_SNIPPET_REPAIR_BATCH) {
        Ok(result) => {
            *cooldown_until = 0;
            result.remaining > 0
        }
        Err(_err) => {
            *cooldown_until = now + INDEX_ERROR_COOLDOWN_SECS;
            false
        }
    }
}

/// One [`DEFAULT_INDEX_BATCH`]-sized pass over `fts_pending` (T-092).
/// Never drains the whole queue itself in one call -- see `run`'s call
/// site for why one bounded batch per loop turn, not a `while` loop to
/// empty, is the point: a `Shutdown`/`FetchBody` sent mid-backlog must be
/// seen between batches, not only after the very last one (requirement
/// 1, requirement 2).
///
/// `cooldown_until` is owned by the caller across loop turns, exactly
/// like `connect_backoff`'s per-account entries a few dozen lines up in
/// `run` -- just global rather than keyed, since `fts_pending` has no
/// `account_id` either (see `search`'s own module doc, "Account
/// isolation"). Requirement 3: on `Err`, this sets `cooldown_until` to
/// `now + `[`INDEX_ERROR_COOLDOWN_SECS`] and returns `false` without
/// touching the queue again until that clears -- one broken row (or a
/// wedged local database) retries at a bounded rate, never spins `run`'s
/// loop at full CPU
/// (`a_failing_index_batch_backs_off_and_never_hot_loops`). The error
/// itself is deliberately never logged, `Debug`-printed, or otherwise
/// inspected here (D14 discipline, matching this file's `Err(_core_err)`
/// arm in `run`'s own `tick_for_account` match a bit further down) even
/// though a `CoreError` from this particular call is SQL-shape text, not
/// message content -- there is no legitimate reason for this worker to
/// ever look inside it.
///
/// Returns `true` when `run` should not fall through into its usual idle
/// wait/`IDLE` round this pass -- either this batch actually indexed
/// something and rows may remain, or a previous pass already left rows
/// behind -- so a large backlog drains across consecutive loop turns at
/// the loop's own pace instead of the idle poll's cadence (requirement
/// 1: draining a 10,000-message backlog must not take hours). `false`
/// covers three different "nothing to hurry back for" cases alike: the
/// queue was already empty, this call is still cooling down from a
/// previous error, or a batch just ran and left nothing behind -- `run`
/// does not need to tell them apart, only whether to skip its sleep.
fn drain_one_index_batch(
    batcher: &impl IndexBatcher,
    bodies_dir: &Path,
    now: i64,
    cooldown_until: &mut i64,
) -> bool {
    if now < *cooldown_until {
        return false;
    }
    match batcher.index_pending_batch(bodies_dir, DEFAULT_INDEX_BATCH) {
        Ok(result) => {
            *cooldown_until = 0;
            result.remaining > 0
        }
        Err(_err) => {
            *cooldown_until = now + INDEX_ERROR_COOLDOWN_SECS;
            false
        }
    }
}

/// What the worker learned happened. Delivered via the callback passed to
/// [`start`], on the worker thread -- see this module's D11 doc comment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncEvent {
    /// A queued operation was confirmed by the server (or the server
    /// reported the same state already in effect, D29) and is `acked` in
    /// the `operations` table.
    Acked { operation_id: OperationId },
    /// A queued operation failed for a reason [`ApplyError::retry`] says
    /// is not worth retrying, and is now `failed` for good.
    Failed {
        operation_id: OperationId,
        error: ApplyError,
    },
    /// Could not open a live session for this account at all (bad/missing
    /// credential, network down, keyring unavailable, ...). `message` is
    /// human text only -- see `connect_error_message`.
    ConnectFailed {
        account_id: AccountId,
        message: String,
    },
    /// One inbound folder sync pass ran (T-078 (b)):
    /// [`feathermail_sync::sync_folder`] was called once against
    /// `folder_id`, and [`Core::record_sync_attempt`] already recorded
    /// whether it succeeded -- `ok` mirrors that same recorded outcome, so
    /// a failure here never crashes the worker or costs this account its
    /// turn; the folder simply waits out its own backoff and is tried
    /// again later. D14: no subject, sender, body, or token -- `folder_id`
    /// is this database's own local `folders.id`, never the IMAP mailbox
    /// path or anything that came off the wire.
    ///
    /// `error` (T-091) is `Some` exactly when `ok` is `false`: which
    /// bucket the failure falls in, so "Diagnostics"/a toast can tell an
    /// auth failure (the worker already dropped the cached session by the
    /// time this fires -- see `run_one_folder_sync`) apart from anything
    /// else, without ever seeing [`feathermail_sync::SyncError`]'s own
    /// `String` payload -- see `sync_failure_reason`, the one function
    /// this worker trusts to decide what crosses that D14 boundary,
    /// mirroring `connect_error_message` just below it.
    /// One inbound folder sync pass is *starting*.
    ///
    /// The owner: "I do not see any preloader showing the progress of
    /// loading the headers for a mailbox." There was none to see -- the
    /// only sync feedback in the shell was the add-account wizard's
    /// spinner, so on an established account a pass over sixty-eight
    /// thousand headers was indistinguishable from the app doing nothing
    /// at all. This is the missing half of [`Self::FolderSynced`]: paired
    /// with it, the shell knows a pass is in flight and for which folder.
    ///
    /// D14, exactly as for `FolderSynced`: `folder_id` is this database's
    /// own local id, never a mailbox path off the wire.
    FolderSyncStarted {
        account_id: AccountId,
        folder_id: String,
    },
    FolderSynced {
        account_id: AccountId,
        folder_id: String,
        ok: bool,
        error: Option<SyncFailureReason>,
    },
    /// T-035/D26: one or more client-local snoozes reached their deadline
    /// and were returned to Inbox. No provider connection is involved; the
    /// event only tells the GTK shell to refresh its cached list.
    SnoozesWoken {
        account_id: AccountId,
        thread_ids: Vec<ThreadId>,
    },
    /// A body fetch requested via [`SyncHandle::fetch_body`] finished
    /// (T-080). `ok` says whether the message's body is now sitting in
    /// the on-disk cache -- either it already was (a `FetchBody` racing a
    /// lookup that just cached it some other way) or [`Core::open_body`]
    /// fetched and stored it -- not whether the bytes are handed along
    /// here, because they are not: D14 keeps message content off this
    /// event/channel entirely. The caller re-reads the cache itself (the
    /// same [`feathermail_core::Core::lookup_body`] call it already used
    /// to discover the miss) once this arrives.
    BodyReady { message_id: MessageId, ok: bool },
    /// An attachment fetch requested via [`SyncHandle::fetch_attachment`]
    /// finished (T-043). The file, filename, MIME type, and any provider
    /// response stay out of this cross-thread event: the caller re-reads
    /// Core's attachment metadata after the completion signal (D14).
    AttachmentReady {
        attachment_id: AttachmentId,
        ok: bool,
    },
}

/// Which bucket a [`SyncEvent::FolderSynced`] failure falls in (T-091).
/// Bare tags, no payload -- the same shape [`ApplyError`] already uses on
/// [`SyncEvent::Failed`], and for the identical D14 reason: a provider's
/// raw error text (the `String` inside
/// [`feathermail_sync::SyncError::Session`]/`Store`) must never leave this
/// module. See `sync_failure_reason`, the only place that decides which
/// variant a given `SyncError` becomes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncFailureReason {
    /// The session's authorization is no longer good
    /// ([`feathermail_sync::SyncError::Auth`]) -- the inbound-sync
    /// analogue of [`ApplyError::Auth`]. By the time this event fires the
    /// worker has already dropped the cached session (see
    /// `run_one_folder_sync`), so the very next attempt against this
    /// account reaches for a fresh one.
    Auth,
    /// Any other sync failure -- network, IMAP protocol, or local
    /// storage. Not broken down further: see
    /// [`feathermail_sync::SyncError`]'s own doc comment for why a finer
    /// split would mean pattern-matching a server's response text, which
    /// this worker is not willing to do.
    Other,
}

/// What the shell currently has on screen, reported through
/// [`SyncHandle::report_viewport`] (T-090). The worker snapshots this
/// before every [`next_sync`] decision and feeds two scheduler inputs
/// from it: [`Focus::open_folder`] and [`PowerState::app_backgrounded`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Viewport {
    /// `folders.id` of the folder currently displayed, `None` whenever
    /// the shell is not on the Inbox screen (Welcome/Add account/
    /// Settings) or shows no folder. One account-independent string is
    /// enough even though the scheduler runs per account: folder ids are
    /// globally unique (`{account}:{slug}`, see `unique_folder_id` in
    /// `crates/core/src/remote.rs`), so another account's
    /// `FolderInput::id` can never accidentally equal it. Overlay
    /// folders (`starred`/`snoozed`, plus `archive`/`trash` when no real
    /// row claims them -- see `FOLDER_SUMMARY_SQL` in
    /// `crates/core/src/store.rs`) match no `FolderInput` at all and
    /// simply degrade to "nothing focused".
    pub open_folder: Option<String>,
    /// `true` when the shell's window is not the active one (minimized
    /// or just unfocused): the user is not looking at this app even if
    /// the machine is otherwise busy. The app itself is the only
    /// possible source of this fact, which is why it rides this report
    /// instead of [`PowerProbe`].
    pub app_backgrounded: bool,
}

/// Command sent to the worker thread. Not exported: callers only ever see
/// [`SyncHandle::wake`] / [`SyncHandle::shutdown`], which is why this can
/// stay a private detail of how those are implemented.
pub(crate) enum WorkerCommand {
    /// New work may be available (UI just dispatched an operation); do not
    /// wait out whatever idle timeout is currently in effect.
    Wake,
    Shutdown,
    /// T-080: `id`'s body was not in the on-disk cache and the shell wants
    /// it fetched over a live session. Carries `account_id` because a
    /// `MessageId` alone does not say whose session to use -- see
    /// [`run`]'s handling: a pending fetch for an account jumps that
    /// account ahead of the plain round robin the next time a connection
    /// is needed, and is served before that connection's queue-draining
    /// turn, since a human is looking at a spinner rather than waiting on
    /// a retry timer.
    FetchBody {
        account_id: AccountId,
        message_id: MessageId,
    },
    /// T-024 batched: warm a set of bodies for one account in a single
    /// `UID FETCH` per folder.
    ///
    /// Deliberately a different command from [`Self::FetchBody`] rather
    /// than a one-element case of it: a click is one message with a person
    /// waiting on it, a warm-up is a set with nobody waiting. They are
    /// served by the same loop but they are not the same request, and the
    /// difference is what keeps the click's latency readable.
    WarmBodies {
        account_id: AccountId,
        message_ids: Vec<MessageId>,
    },
    /// T-043: download one attachment to Core's deterministic cache path
    /// through the account's live session. As for [`Self::FetchBody`],
    /// `account_id` selects the correct connection and gives this
    /// user-requested work priority over routine synchronization.
    FetchAttachment {
        account_id: AccountId,
        attachment_id: AttachmentId,
    },
}

/// Private startup barrier used only by T-067's first-frame ordering.
///
/// The ordinary [`start`] path has no barrier. [`start_deferred`] gives the
/// GTK shell a real [`SyncHandle`] while the worker is still dormant, so the
/// shell can map its first frame before the worker is allowed to open SQLite
/// or a provider connection. `Shutdown` is separate from
/// [`WorkerCommand::Shutdown`]: a dormant worker has not entered [`run`] and
/// therefore is not reading the normal command channel yet.
enum StartGate {
    Activate,
    Shutdown,
}

/// A user-visible cache miss waiting for the account's live session.
///
/// Body and attachment fetches deliberately share one FIFO queue: each is
/// foreground work and each must interrupt an IDLE wait, but the worker
/// serves only one request per account turn so a burst cannot starve its
/// ordinary operation queue or folder synchronization.
#[derive(Clone, Debug, PartialEq, Eq)]
enum PendingFetch {
    Body {
        account_id: AccountId,
        message_id: MessageId,
    },
    /// T-024 batched: the warm-up's set, served in one turn of the loop.
    Warm {
        account_id: AccountId,
        message_ids: Vec<MessageId>,
    },
    Attachment {
        account_id: AccountId,
        attachment_id: AttachmentId,
    },
}

impl PendingFetch {
    fn account_id(&self) -> &AccountId {
        match self {
            Self::Body { account_id, .. }
            | Self::Warm { account_id, .. }
            | Self::Attachment { account_id, .. } => account_id,
        }
    }

    fn emit_ready(self, events: &(dyn Fn(SyncEvent) + Send), ok: bool) {
        match self {
            Self::Body { message_id, .. } => events(SyncEvent::BodyReady { message_id, ok }),
            // A batch reports per message, so this whole-request `ok` only
            // fires the blanket case (the fetch never happened at all).
            // The served path reports each id with its own result instead.
            Self::Warm { message_ids, .. } => {
                for message_id in message_ids {
                    events(SyncEvent::BodyReady { message_id, ok });
                }
            }
            Self::Attachment { attachment_id, .. } => {
                events(SyncEvent::AttachmentReady { attachment_id, ok });
            }
        }
    }

    /// A person is waiting on this one -- reading pane or Save/Open --
    /// as opposed to a warm-up filling a cache nobody asked for.
    fn is_foreground(&self) -> bool {
        !matches!(self, Self::Warm { .. })
    }
}

/// Which account a pending queue wants the worker to speak for next.
///
/// The merged view (T-108) queues fetches against more than one mailbox,
/// and this worker has one live session. FIFO would serve the warm-up
/// that started when the folder opened and leave a click parked until
/// that account's IDLE slice ended -- which is the letter that never
/// loads. A click (`Body`) or an attachment download outranks a warm-up;
/// only when nothing foreground is waiting does the queue stay FIFO.
fn preferred_pending_account(queue: &VecDeque<PendingFetch>) -> Option<AccountId> {
    queue
        .iter()
        .find(|request| request.is_foreground())
        .or_else(|| queue.front())
        .map(|request| request.account_id().clone())
}

/// What to do with the live session given the pending queue.
///
/// Serving a warm-up for the connected account while a click is waiting
/// on another mailbox is the merged-view failure: the click sits in
/// `pending_fetches` (so IDLE's `should_stop` will not see it on `rx`)
/// and then waits out the whole IDLE slice. Dropping the session hands
/// the next pass to [`preferred_pending_account`], which picks the click.
#[derive(Debug, PartialEq, Eq)]
enum ConnectedFetch {
    Serve(usize),
    SwitchAccount,
    None,
}

fn connected_fetch_action(
    queue: &VecDeque<PendingFetch>,
    account_id: &AccountId,
) -> ConnectedFetch {
    if let Some(idx) = queue
        .iter()
        .position(|request| request.account_id() == account_id && request.is_foreground())
    {
        return ConnectedFetch::Serve(idx);
    }
    if queue
        .iter()
        .any(|request| request.is_foreground() && request.account_id() != account_id)
    {
        return ConnectedFetch::SwitchAccount;
    }
    if let Some(idx) = queue
        .iter()
        .position(|request| request.account_id() == account_id)
    {
        return ConnectedFetch::Serve(idx);
    }
    ConnectedFetch::None
}

/// Handle to the background worker (T-078). Cheap to hold, safe to call from
/// any thread (including the GTK main thread -- see this module's D11 doc
/// comment: this handle's methods are the *only* contact point, never
/// `Core`/`ProviderFactory` directly). A [`start_deferred`] handle is parked
/// at its startup gate until [`Self::activate`].
pub struct SyncHandle {
    tx: mpsc::Sender<WorkerCommand>,
    join: Option<JoinHandle<()>>,
    /// `Some` only for [`start_deferred`]. Keeping the gate in the handle
    /// makes close-before-first-frame safe: [`Drop`] can release the parked
    /// worker without waiting on the GTK thread.
    start_gate: Option<mpsc::Sender<StartGate>>,
    /// T-090: the shared cell [`Self::report_viewport`] writes and the
    /// worker snapshots before every [`next_sync`] decision. A cell, not
    /// a `WorkerCommand`, so a focus change can never be swallowed by
    /// one of this module's several independent `rx` drains before the
    /// scheduler sees it -- see this module's own T-090 doc comment.
    viewport: Arc<Mutex<Viewport>>,
    /// Explicit server refreshes requested by the shell. This is shared
    /// state, rather than a `WorkerCommand`, for the same reason as
    /// `viewport`: several independent command drains must be able to wake
    /// the worker without accidentally consuming and forgetting the work.
    refresh_accounts: Arc<Mutex<VecDeque<AccountId>>>,
}

impl SyncHandle {
    /// Let a [`start_deferred`] worker begin its first sync pass.
    ///
    /// Returns `true` exactly once for a deferred handle. It is deliberately
    /// an inexpensive sender operation, so the GTK map callback never opens
    /// a profile or performs network work itself (D11).
    pub fn activate(&mut self) -> bool {
        let Some(gate) = self.start_gate.take() else {
            return false;
        };
        gate.send(StartGate::Activate).is_ok()
    }

    /// Wake the worker immediately instead of waiting out its current idle
    /// timeout. Call this right after dispatching a new operation --
    /// without it, a freshly-queued operation would sit until the
    /// worker's next scheduled wake-up (up to [`MAX_IDLE_POLL_SECS`]).
    /// Never blocks.
    pub fn wake(&self) {
        let _ = self.tx.send(WorkerCommand::Wake);
    }

    /// Force one inbound folder pass for each requested account, regardless
    /// of the normal scheduler deadline. Duplicate pulls coalesce while an
    /// earlier request is still pending. Never blocks on network work.
    pub fn refresh(&self, account_ids: impl IntoIterator<Item = AccountId>) {
        if let Ok(mut pending) = self.refresh_accounts.lock() {
            for account_id in account_ids {
                if !pending.contains(&account_id) {
                    pending.push_back(account_id);
                }
            }
        }
        let _ = self.tx.send(WorkerCommand::Wake);
    }

    /// Ask the worker to fetch one message's body over a live session
    /// (T-080), for a caller that already checked
    /// [`feathermail_core::Core::lookup_body`] itself and got
    /// [`feathermail_core::body::BodyLookup::NotCached`] back. Never
    /// blocks. Like [`Self::wake`], a missing or already-shut-down worker
    /// degrades to a silent no-op rather than a panic -- the caller (the
    /// GTK shell) is expected to show its own "no cache, no network" state
    /// rather than wait forever for an event that will never arrive; see
    /// `crates/app/src/shell.rs`'s `BodyState`.
    pub fn fetch_body(&self, account_id: AccountId, message_id: MessageId) {
        let _ = self.tx.send(WorkerCommand::FetchBody {
            account_id,
            message_id,
        });
    }

    /// T-024 batched: ask for a set of bodies for one account in one go.
    ///
    /// Never blocks and carries no bytes back, exactly like
    /// [`Self::fetch_body`]; each message is reported on its own
    /// [`SyncEvent::BodyReady`], so a caller that queued twenty ids hears
    /// twenty answers and does not have to guess which of them landed.
    pub fn warm_bodies(&self, account_id: AccountId, message_ids: Vec<MessageId>) {
        if message_ids.is_empty() {
            return;
        }
        let _ = self.tx.send(WorkerCommand::WarmBodies {
            account_id,
            message_ids,
        });
    }

    /// Ask the worker to download one attachment into Core's on-disk cache
    /// (T-043). Never blocks and never carries attachment bytes back over
    /// this channel: completion is reported as [`SyncEvent::AttachmentReady`].
    pub fn fetch_attachment(&self, account_id: AccountId, attachment_id: AttachmentId) {
        let _ = self.tx.send(WorkerCommand::FetchAttachment {
            account_id,
            attachment_id,
        });
    }

    /// T-090: tell the worker what is on screen right now (which folder,
    /// whether the window is backgrounded), so [`next_sync`] stops
    /// seeing [`Focus::default`]/[`PowerState::default`] for the two
    /// inputs only the shell can know. The shell dedupes, so this is
    /// cheap to call on every state change that *might* matter; even a
    /// duplicate costs one mutex write plus one `Wake`, which every wait
    /// path already treats as a no-op nudge.
    ///
    /// Never blocks: the channel send is the same lossless-but-
    /// drop-safe one [`Self::wake`] uses (a dead worker turns it into a
    /// silent no-op), and the worker holds the mutex only for the
    /// microseconds a snapshot clone takes. The `Wake` is sent even if
    /// the write lost to a poisoned lock (i.e. the worker panicked) --
    /// waking a dead channel is harmless, and a parked live worker is
    /// the overwhelmingly common case: without the nudge it would sit
    /// out its current hint (up to 900 simulated seconds for a
    /// background-tier folder) before noticing the focus changed.
    pub fn report_viewport(&self, viewport: Viewport) {
        if let Ok(mut slot) = self.viewport.lock() {
            *slot = viewport;
        }
        let _ = self.tx.send(WorkerCommand::Wake);
    }

    /// Stop the worker and join its thread. Blocks until the thread is
    /// actually gone: the worker checks for shutdown between ticks and
    /// while waiting, so this is fast when it is idle, but it can still
    /// take as long as the network call it is currently inside. **Never
    /// call this from the GTK thread** (D11) -- dropping the handle sends
    /// the same command without waiting, which is what the shell relies
    /// on; see [`Drop`](Self::drop).
    pub fn shutdown(mut self) {
        if let Some(gate) = self.start_gate.take() {
            let _ = gate.send(StartGate::Shutdown);
        }
        let _ = self.tx.send(WorkerCommand::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for SyncHandle {
    /// Tells the worker to stop, and deliberately does **not** wait for it.
    ///
    /// The GTK shell holds this handle for as long as its window lives
    /// (`crates/app/src/shell.rs`'s `App::sync_handle`), so `drop` runs on
    /// the GTK thread while the window is closing. Joining there would
    /// block that thread for however long the worker's current network
    /// call takes -- up to `feathermail_providers::wire::TIMEOUT` (8s) per
    /// socket operation, more across a TLS handshake -- and the user would
    /// watch a window that refuses to close. That is exactly the stall
    /// D11 exists to forbid.
    ///
    /// Not joining is safe here, and not merely tolerable:
    /// - D31: an operation killed mid-apply is left `running`, and the
    ///   next `Core::open` turns it back into `pending` (`recover_inflight`
    ///   runs both there and at the top of [`run`]). Losing the tail of an
    ///   apply costs one retry, not a lost operation.
    /// - The worker keeps no unflushed local state: every change it makes
    ///   is a committed SQLite transaction, and WAL survives the process
    ///   dying outright, never mind a thread being left behind.
    /// - The shutdown command still goes out, so a worker that outlives
    ///   this handle (a caller tearing the worker down while the process
    ///   keeps running) exits at its next loop check -- at most one
    ///   network call later -- instead of running forever.
    ///
    /// [`Self::shutdown`] is the way to actually wait, for callers that
    /// need to know the thread is gone (the tests in this module do) --
    /// and it takes `self`, so it has already taken `join` and this finds
    /// nothing left to do.
    fn drop(&mut self) {
        if let Some(gate) = self.start_gate.take() {
            let _ = gate.send(StartGate::Shutdown);
        }
        let _ = self.tx.send(WorkerCommand::Shutdown);
    }
}

/// Where the worker learns [`PowerState::on_battery`] from (T-090). One
/// method, not a full `PowerState`, because the other three fields have
/// a different source (`app_backgrounded` -- the shell's own [`Viewport`]
/// report) or no source at all yet (`screen_locked_or_idle`,
/// `network_metered`: session D-Bus / NetworkManager, neither linked by
/// this crate today).
pub(crate) trait PowerProbe: Send {
    /// Best-effort "running on battery right now". `false` must also be
    /// the answer to "cannot tell" (no battery hardware, unreadable
    /// sysfs): the non-power-saving cadence is what every release before
    /// T-090 did, so it is the honest fallback, and a flaky probe must
    /// not be able to mute sync entirely.
    fn on_battery(&self) -> bool;
}

/// Production [`PowerProbe`]: the Linux sysfs power-supply class. This is
/// the same data UPower itself reads, minus a D-Bus dependency this crate
/// does not otherwise have. `root` is a field (not a hardcoded path)
/// purely so tests can point it at a tempdir fixture.
struct SysfsPowerProbe {
    root: PathBuf,
}

impl SysfsPowerProbe {
    fn system() -> Self {
        Self {
            root: PathBuf::from("/sys/class/power_supply"),
        }
    }
}

impl PowerProbe for SysfsPowerProbe {
    /// `true` iff at least one power supply of `type` `Battery` reports
    /// `status` `Discharging`. A desktop with no battery entries, an
    /// unreadable `root`, and batteries that are `Charging`/`Full`/`Not
    /// charging` all read as "not on battery" -- see the trait's doc
    /// comment for why "cannot tell" must fold into `false`.
    fn on_battery(&self) -> bool {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return false;
        };
        for entry in entries.flatten() {
            let supply_type =
                std::fs::read_to_string(entry.path().join("type")).unwrap_or_default();
            if supply_type.trim() != "Battery" {
                continue;
            }
            let status = std::fs::read_to_string(entry.path().join("status")).unwrap_or_default();
            if status.trim() == "Discharging" {
                return true;
            }
        }
        false
    }
}

/// T-090: where [`run`] gets `next_sync`'s non-database inputs from --
/// the shell's shared [`Viewport`] cell and the system [`PowerProbe`].
/// Bundled into one struct because `run`'s parameter list was already at
/// clippy's `too_many_arguments` threshold before T-090, and these two
/// always travel together into exactly one place: the per-pass
/// [`ScheduleSnapshot::capture`].
struct SchedulerSources {
    viewport: Arc<Mutex<Viewport>>,
    refresh_accounts: Arc<Mutex<VecDeque<AccountId>>>,
    power: Box<dyn PowerProbe>,
}

/// T-090: one consistent read of the world for one [`next_sync`]
/// decision -- the worker clock's `now`, the shell's viewport, and the
/// power state, all captured at the same instant, so a decision never
/// mixes, say, the folder the user opened at T with the battery state at
/// T+5s. Bundled (rather than three extra parameters) to keep
/// [`sync_one_due_folder`] under clippy's `too_many_arguments` threshold
/// without an `#[allow]`.
struct ScheduleSnapshot {
    now: i64,
    view: Viewport,
    power: PowerState,
}

impl ScheduleSnapshot {
    /// Snapshots both sources at once. The viewport lock is held only for
    /// the clone, so [`SyncHandle::report_viewport`] on the GTK thread
    /// never waits on anything the worker does with the snapshot
    /// afterwards. A poisoned lock degrades to the neutral default --
    /// exactly pre-T-090 behavior -- rather than crashing the loop (the
    /// only writer is the handle, and nothing here panics with the lock
    /// held, so this is belt-and-braces, not a real path).
    fn capture(now: i64, sources: &SchedulerSources) -> Self {
        let view = sources
            .viewport
            .lock()
            .map(|v| v.clone())
            .unwrap_or_default();
        let power = PowerState {
            on_battery: sources.power.on_battery(),
            // No source today: session D-Bus (logind) is not linked by
            // this crate. `false` = pre-T-090 cadence, the honest
            // fallback.
            screen_locked_or_idle: false,
            app_backgrounded: view.app_backgrounded,
            // No source today either (NetworkManager D-Bus).
            network_metered: false,
        };
        Self { now, view, power }
    }

    fn focus(&self) -> Focus<'_> {
        Focus {
            open_folder: self.view.open_folder.as_deref(),
            // The shell only ever displays an open thread inside the
            // folder it is also showing (`OpenSearchHit` switches the
            // displayed folder to the hit's own first), so there is no
            // second folder signal to feed here today -- see this
            // module's T-090 doc comment.
            open_thread_folder: None,
        }
    }
}

fn next_refresh_account(sources: &SchedulerSources) -> Option<AccountId> {
    sources
        .refresh_accounts
        .lock()
        .ok()
        .and_then(|pending| pending.front().cloned())
}

fn finish_refresh_account(sources: &SchedulerSources, account_id: &AccountId) {
    if let Ok(mut pending) = sources.refresh_accounts.lock() {
        if pending.front() == Some(account_id) {
            pending.pop_front();
        }
    }
}

/// Starts the background sync worker on its own thread (T-078, D12) and
/// returns a handle to it. `db_path` is opened independently by the
/// worker itself (its own [`Core::open`], separate from whatever handle(s)
/// the GTK shell holds -- see `crates/app/src/shell.rs`'s
/// `open_core_handles` for the same split applied to the shell's own
/// reader/writer pair) so this crate never shares a `Core` -- and
/// therefore never shares a lock -- with the UI thread (D11).
///
/// `events` is called on the worker thread for every [`SyncEvent`]; the
/// caller is responsible for hopping back to the GTK main loop if it needs
/// to touch widgets.
pub fn start(
    db_path: impl Into<PathBuf>,
    factory: impl ProviderFactory + 'static,
    events: impl Fn(SyncEvent) + Send + 'static,
) -> SyncHandle {
    start_with_clock(
        db_path,
        factory,
        events,
        SystemClock,
        SysfsPowerProbe::system(),
    )
}

/// Like [`start`], but holds the worker before its first [`run`] until the
/// caller invokes [`SyncHandle::activate`].
///
/// T-067 uses this for the GTK shell: it creates and keeps the same normal
/// background-sync handle during `App::init`, then activates it only after
/// GTK has mapped the first window and yielded one idle turn. Consequently no
/// `Core::open` or provider connection can begin before that visible-shell
/// boundary. A deferred handle remains safe to drop or [`SyncHandle::shutdown`]
/// before activation.
pub fn start_deferred(
    db_path: impl Into<PathBuf>,
    factory: impl ProviderFactory + 'static,
    events: impl Fn(SyncEvent) + Send + 'static,
) -> SyncHandle {
    let (gate_tx, gate_rx) = mpsc::channel();
    start_with_clock_and_gate(
        db_path,
        factory,
        events,
        SystemClock,
        SysfsPowerProbe::system(),
        Some((gate_tx, gate_rx)),
    )
}

/// [`start`] with an injectable clock/waiter and power probe. Not part of
/// the public API: production always uses [`SystemClock`] and
/// [`SysfsPowerProbe`], and the only other implementations
/// ([`tests::FakeClock`], `tests::FixedPower`) exist purely so this
/// crate's own tests can prove the D32 backoff table and the T-090 power
/// adjustment are honored without a test actually sleeping out
/// multi-second real delays or needing a real battery. `pub(crate)`
/// rather than `#[cfg(test)]`-gated because `worker`'s tests live in this
/// same module (they need [`WorkerCommand`], which is private) rather
/// than as a separate integration test crate.
pub(crate) fn start_with_clock(
    db_path: impl Into<PathBuf>,
    factory: impl ProviderFactory + 'static,
    events: impl Fn(SyncEvent) + Send + 'static,
    clock: impl WorkerClock + 'static,
    power: impl PowerProbe + 'static,
) -> SyncHandle {
    start_with_clock_and_gate(db_path, factory, events, clock, power, None)
}

fn start_with_clock_and_gate(
    db_path: impl Into<PathBuf>,
    factory: impl ProviderFactory + 'static,
    events: impl Fn(SyncEvent) + Send + 'static,
    clock: impl WorkerClock + 'static,
    power: impl PowerProbe + 'static,
    start_gate: Option<(mpsc::Sender<StartGate>, mpsc::Receiver<StartGate>)>,
) -> SyncHandle {
    let db_path = db_path.into();
    let (tx, rx) = mpsc::channel();
    // Cloned before `tx` moves into the closure below: `run` sends itself
    // `Wake` on this clone when `drain_one_index_batch` leaves rows behind,
    // so an indexing backlog gets the loop's next turn immediately instead
    // of waiting out `wait_for_shutdown`/IDLE's full sleep (T-092,
    // requirement 1). See `run`'s own `self_tx` parameter doc.
    let self_tx = tx.clone();
    // T-090: the one shared cell both sides of the handle/worker boundary
    // hold -- see `SyncHandle::report_viewport` and this module's own
    // T-090 doc comment for why this is a cell and not a `WorkerCommand`.
    let viewport = Arc::new(Mutex::new(Viewport::default()));
    let viewport_for_worker = Arc::clone(&viewport);
    let refresh_accounts = Arc::new(Mutex::new(VecDeque::new()));
    let refresh_accounts_for_worker = Arc::clone(&refresh_accounts);
    let (start_gate, gate_rx) = match start_gate {
        Some((gate_tx, gate_rx)) => (Some(gate_tx), Some(gate_rx)),
        None => (None, None),
    };
    let join = std::thread::Builder::new()
        .name("feathermail-sync".into())
        .spawn(move || {
            if let Some(gate_rx) = gate_rx {
                if !matches!(gate_rx.recv(), Ok(StartGate::Activate)) {
                    return;
                }
            }
            run(
                db_path,
                Box::new(factory),
                &events,
                rx,
                self_tx,
                clock,
                SchedulerSources {
                    viewport: viewport_for_worker,
                    refresh_accounts: refresh_accounts_for_worker,
                    power: Box::new(power),
                },
            )
        })
        .expect("spawning the background sync thread must not fail");
    SyncHandle {
        tx,
        join: Some(join),
        start_gate,
        viewport,
        refresh_accounts,
    }
}

/// Abstracts "what time is it" and "block until woken or a timeout
/// elapses" so the worker's backoff logic can be driven by a fake in
/// tests. See [`SystemClock`] (production) and `tests::FakeClock` (tests).
pub(crate) trait WorkerClock: Send {
    /// Unix seconds, fed straight into [`Core::set_now`] before every tick
    /// so the queue's own `next_attempt_at` comparisons use the same clock
    /// this worker's waits do.
    fn now(&self) -> i64;

    /// Block for up to `timeout_secs`, returning early with whichever
    /// command arrived first, or `None` if the timeout elapsed with
    /// nothing sent.
    fn wait(&self, rx: &Receiver<WorkerCommand>, timeout_secs: i64) -> Option<WorkerCommand>;
}

struct SystemClock;

impl WorkerClock for SystemClock {
    fn now(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    fn wait(&self, rx: &Receiver<WorkerCommand>, timeout_secs: i64) -> Option<WorkerCommand> {
        let secs = timeout_secs.clamp(0, MAX_IDLE_POLL_SECS) as u64;
        rx.recv_timeout(Duration::from_secs(secs)).ok()
    }
}

/// The worker loop itself. Runs entirely on the thread [`start_with_clock`]
/// spawned; never returns until told to shut down (or the profile cannot
/// be opened at all).
fn run(
    db_path: PathBuf,
    mut factory: Box<dyn ProviderFactory>,
    events: &(dyn Fn(SyncEvent) + Send),
    rx: Receiver<WorkerCommand>,
    // T-092: a clone of the very `Sender` `rx` reads from. Its only use is
    // `drain_one_index_batch` telling this loop "more `fts_pending` rows
    // are waiting" -- sent as a `WorkerCommand::Wake`, so every existing
    // wait primitive below (`wait_for_shutdown`, real IMAP `IDLE` via
    // `run_idle_with`, which polls `should_stop` before blocking) already
    // knows how to return early for it, with no second flag threaded
    // through each call site.
    self_tx: Sender<WorkerCommand>,
    clock: impl WorkerClock,
    // T-090: the shell's latest "what is on screen" report plus the
    // system power probe -- the two sources `next_sync`'s
    // `Focus`/`PowerState` inputs now come from instead of hardcoded
    // defaults, snapshotted via `ScheduleSnapshot::capture` before every
    // `sync_one_due_folder` call below. See this module's own T-090 doc
    // comment.
    sources: SchedulerSources,
) {
    let mut core = match Core::open(&db_path) {
        Ok(core) => core,
        Err(err) => {
            // D14: `CoreError::message` is already human/non-secret (same
            // guarantee `crates/app/src/shell.rs`'s `open_core_handles`
            // relies on for its own analogous `eprintln`). Nothing to
            // recover into here -- there is no profile to serve -- so the
            // thread just ends; `SyncHandle::wake`/`shutdown` degrade to
            // harmless no-ops against a closed channel.
            eprintln!(
                "feathermail-service: could not open {}: {}",
                db_path.display(),
                err.message
            );
            return;
        }
    };
    // `Core::open` already ran `recover_inflight` (D31) as part of opening.
    // Calling it again here is a cheap, idempotent no-op (`UPDATE ... WHERE
    // status = 'running'` against a table that already has none) -- kept
    // explicit at this call site so the guarantee this worker depends on
    // (a crash mid-apply is retried, not stuck) does not quietly rely on a
    // side effect buried inside a constructor a future change could drop.
    let _ = core.recover_inflight();

    let mut connected: Option<(AccountId, Box<dyn MailSession>)> = None;
    // Connect backoff is **per account**, not global. One unreachable
    // account (revoked credential, host that no longer resolves) must not
    // make a healthy account wait out its delay, and must not be retried
    // on every single pass of the round-robin -- that would be a reconnect
    // attempt, and a `ConnectFailed` event, every time any other account
    // went idle. Keyed by account id: failure count for the D32 table, and
    // the instant this account is allowed to be tried again.
    let mut connect_backoff: HashMap<String, (u32, i64)> = HashMap::new();
    let mut account_cursor: usize = 0;
    let mut known_delay: Option<i64> = None;
    // How many accounts the profile had the last time we picked one, and
    // how many of them have come back idle in a row since the last time
    // any of them actually did something. Together they answer "has every
    // account had its turn?" -- the only point at which a per-account
    // `Idle` justifies sleeping (see this module's doc comment).
    let mut account_count: usize = 1;
    let mut idle_streak: usize = 0;
    // Shortest wait any of the accounts in the current idle sweep asked
    // for. A retry scheduled 2s out on one account must not be buried
    // under another account's 15-minute idle poll.
    let mut idle_wait: i64 = MAX_IDLE_POLL_SECS;
    // T-080/T-043: the on-disk cache miss path. The shell sends either a
    // body or attachment request only after it finds the cache missing;
    // by the time an entry lands here it is already known to need a live
    // session. `RefCell`, not a local `let mut`, because
    // both `wait_for_shutdown` and `sync_one_due_folder` (below) need to
    // push into it from inside an `is_cancelled`/wait call that only
    // borrows immutably otherwise -- same shape as `shutdown_seen`'s
    // `AtomicBool` a few functions down, just for a queue instead of a
    // flag.
    let bodies_dir = default_bodies_dir();
    let attachments_dir = default_attachments_dir();
    let pending_fetches: RefCell<VecDeque<PendingFetch>> = RefCell::new(VecDeque::new());
    // T-092: `now` this worker is allowed to try `index_pending_batch`
    // again after it last returned `Err`. `0` means "not cooling down" --
    // see `drain_one_index_batch`'s own doc for why this lives here
    // (across loop turns, unkeyed) rather than inside that function.
    let mut index_cooldown_until: i64 = 0;
    // T-134: same shape, its own deadline -- see
    // `drain_one_snippet_repair_batch`.
    let mut snippet_repair_cooldown_until: i64 = 0;

    loop {
        let now = clock.now();
        core.set_now(now);

        // Snooze is local-only and must wake even while every account is
        // offline. Run this before connection selection so a deadline never
        // waits for IMAP authentication, and emit only ids (D14).
        if let Ok(woken) = core.wake_due_snoozes() {
            for (account_id, thread_id) in woken {
                events(SyncEvent::SnoozesWoken {
                    account_id,
                    thread_ids: vec![thread_id],
                });
            }
        }

        // Drain anything sent while this pass was busy elsewhere (ticking
        // the queue, running one sync pass) rather than blocked in
        // `wait_for_shutdown` -- those calls handle their own draining,
        // but the fast paths below (`Acked`/`Retry`/`Failed`, which loop
        // immediately with no wait at all) never otherwise touch `rx`, so
        // a fetch request sent during one of those would sit unread
        // forever. `Wake` needs no handling: the loop is already about to
        // go around again regardless of why.
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                WorkerCommand::Shutdown => return,
                WorkerCommand::FetchBody {
                    account_id,
                    message_id,
                } => {
                    pending_fetches.borrow_mut().push_back(PendingFetch::Body {
                        account_id,
                        message_id,
                    });
                }
                WorkerCommand::WarmBodies {
                    account_id,
                    message_ids,
                } => {
                    pending_fetches.borrow_mut().push_back(PendingFetch::Warm {
                        account_id,
                        message_ids,
                    });
                }
                WorkerCommand::FetchAttachment {
                    account_id,
                    attachment_id,
                } => {
                    pending_fetches
                        .borrow_mut()
                        .push_back(PendingFetch::Attachment {
                            account_id,
                            attachment_id,
                        });
                }
                WorkerCommand::Wake => {}
            }
        }

        // T-092: exactly one `DEFAULT_INDEX_BATCH`-sized pass over
        // `fts_pending` per loop turn, unconditionally -- not gated behind
        // `connected`/`pick_account` the way `sync_one_due_folder` below
        // is, because indexing already-synced mail is purely local disk
        // and SQLite work with no `account_id` of its own (see `search`'s
        // module doc, "Account isolation") and must keep making progress
        // even while every account is sitting in connect backoff. Never a
        // `while` loop draining to empty here: that would let a large
        // backlog starve `rx` (a `Shutdown` or `FetchBody` sent mid-drain
        // would not be seen until the whole queue emptied) -- see this
        // function's `self_tx`, the `rx.try_recv()` drain just above (this
        // call site sits *after* it deliberately: a self-`Wake` sent
        // before that drain would be swallowed by it as a same-iteration
        // no-op instead of surviving to interrupt whatever wait this same
        // iteration reaches further down -- caught by
        // `a_backlog_bigger_than_one_batch_drains_across_several_loop_turns_not_just_the_first`
        // hanging in real time when this call site was first placed
        // before the drain instead of after it), and `drain_one_index_
        // batch`'s doc for how rows left behind get the *next* turn
        // instead of the idle poll's cadence.
        if drain_one_index_batch(&core, &bodies_dir, clock.now(), &mut index_cooldown_until) {
            let _ = self_tx.send(WorkerCommand::Wake);
        }

        // T-134: and one pass over `snippet_repairs`, for the same reasons
        // and with the same "rows left behind get the next turn" handshake.
        if drain_one_snippet_repair_batch(
            &core,
            &bodies_dir,
            clock.now(),
            &mut snippet_repair_cooldown_until,
        ) {
            let _ = self_tx.send(WorkerCommand::Wake);
        }

        if connected.is_none() {
            let now = clock.now();
            // An account a pending foreground fetch is waiting on jumps the
            // round robin: a click in the reading pane is a person
            // waiting right now, not a background sync that can wait its
            // turn. `pick_account`'s own cursor/backoff bookkeeping is
            // simply skipped this pass, not corrupted -- the next
            // iteration with no pending fetch resumes the round robin
            // exactly where `pick_account`'s internal cursor already was.
            let selected = preferred_pending_account(&pending_fetches.borrow())
                .or_else(|| next_refresh_account(&sources))
                .or_else(|| {
                    pick_account(
                        &core,
                        &mut account_cursor,
                        &mut account_count,
                        &mut connect_backoff,
                        now,
                    )
                });
            match selected {
                None => {
                    // No account is connectable right now: either the
                    // profile has none at all yet (fresh install, Welcome
                    // screen), or every one of them is serving a connect
                    // backoff. In the second case wait exactly until the
                    // earliest of them comes due, not a flat poll -- a
                    // two-second backoff must not be buried under fifteen
                    // minutes of idle.
                    let until_due = soonest_retry(&connect_backoff, now);
                    let snooze_due = core
                        .next_snooze_deadline(None)
                        .ok()
                        .flatten()
                        .map(|deadline| (deadline - now).max(0));
                    let wait = [
                        known_delay.take(),
                        until_due,
                        snooze_due,
                        Some(MAX_IDLE_POLL_SECS),
                    ]
                    .into_iter()
                    .flatten()
                    .min()
                    .unwrap_or(MAX_IDLE_POLL_SECS)
                    // Floor of one second. Nothing here is supposed to
                    // produce a zero, but this arm is the one place where a
                    // zero would not merely waste a pass: `clock.wait(.., 0)`
                    // degrades to `recv_timeout(0)`, so the loop would come
                    // straight back, re-run `wake_due_snoozes` and three
                    // more queries, and ask for zero again -- a tight loop
                    // against the profile database for as long as the
                    // process lives (D30: "никакого tight loop"). The
                    // shortest real deadline anything can ask for is the
                    // D32 two-second backoff, so a one-second floor cannot
                    // delay a legitimate wake-up.
                    .max(1);
                    if wait_for_shutdown(&rx, &clock, wait, &pending_fetches) {
                        return;
                    }
                    continue;
                }
                Some(account_id) => match factory.connect(&account_id).and_then(|mut provider| {
                    bootstrap_folders_if_needed(&mut core, &account_id, &mut *provider)?;
                    Ok(provider)
                }) {
                    Ok(provider) => {
                        connect_backoff.remove(account_id.as_str());
                        connected = Some((account_id, provider));
                    }
                    Err(err) => {
                        let entry = connect_backoff
                            .entry(account_id.0.clone())
                            .or_insert((0, now));
                        entry.0 += 1;
                        let delay = feathermail_sync::backoff::backoff_delay_secs(entry.0);
                        entry.1 = now + delay;
                        // T-080/T-043: a connect failure means "no live
                        // session for this account right now" -- resolve
                        // every pending fetch queued against it with `ok:
                        // false` immediately rather than leaving the
                        // shell's reading pane on "Loading" until this
                        // account's backoff happens to clear on its own.
                        // `retain` both filters and emits in one pass;
                        // `events` is `Fn`, not `FnMut`, so calling it
                        // from inside the closure is fine.
                        pending_fetches.borrow_mut().retain(|request| {
                            if request.account_id() == &account_id {
                                request.clone().emit_ready(events, false);
                                false
                            } else {
                                true
                            }
                        });
                        // A manual refresh is one bounded attempt, not a
                        // request to bypass D32 and reconnect in a hot loop.
                        finish_refresh_account(&sources, &account_id);
                        events(SyncEvent::ConnectFailed {
                            account_id,
                            message: connect_error_message(&err),
                        });
                        // Do not wait here: another account may be
                        // connectable right now, and if none is, the
                        // `None` arm above computes the wait from these
                        // very deadlines on the next pass.
                        continue;
                    }
                },
            }
        }

        // T-080/T-043: serve at most one pending foreground fetch for
        // this account before ticking its operation queue -- a click in
        // the reading pane should not sit behind every already-queued
        // Send/MarkRead for this account. At most one per pass, same "no
        // starvation" shape as `sync_one_due_folder` below: a burst of
        // clicks must not let fetches monopolize this account's turn
        // either.
        // T-024 batched: a click outranks a warm-up. Both are "pending
        // fetches for this account", but one has a person watching a
        // spinner and the other is filling a cache nobody has asked for
        // yet -- so a single-message request is taken first even if a
        // batch was queued before it. Without this the batch would be
        // exactly the head-of-line wait it was introduced to shorten.
        // T-115: a click for *another* mailbox drops this session rather
        // than waiting out this account's remaining turn. The merged view
        // queues both, and IDLE's `should_stop` only sees new commands on
        // `rx` -- a FetchBody already in `pending_fetches` would otherwise
        // sit through the whole IDLE slice, which is the letter that
        // never loads.
        // Split into two statements rather than one chained expression:
        // the `Ref` a `.borrow()` returns is a temporary whose lifetime
        // Rust extends to the end of its enclosing statement, so folding
        // the `.borrow_mut()` below into the same `let` (via `.and_then`)
        // would hold the read borrow open across it and panic ("already
        // borrowed") the moment a fetch is actually pending -- exactly
        // the case `fetch_body_serves_a_pending_fetch_and_emits_body_ready`
        // exists to exercise.
        let current_id = connected
            .as_ref()
            .expect("connected was just ensured Some above")
            .0
            .clone();
        let fetch_action = connected_fetch_action(&pending_fetches.borrow(), &current_id);
        match fetch_action {
            ConnectedFetch::SwitchAccount => {
                crate::bodylog!(
                    "pass account={} queue={} switching to a waiting click",
                    current_id.0,
                    pending_fetches.borrow().len()
                );
                connected = None;
                continue;
            }
            ConnectedFetch::Serve(idx) => {
                let request = pending_fetches
                    .borrow_mut()
                    .remove(idx)
                    .expect("connected_fetch_action just found this index");
                let (account_id, provider) = connected
                    .as_mut()
                    .expect("connected was just ensured Some above");
                let started = std::time::Instant::now();
                // T-024 batched: a warm-up set reports per message, so it does
                // not go through `emit_ready`'s single verdict -- half a batch
                // succeeding is the normal case (an expunged UID, a folder that
                // has moved) and the shell has to be told exactly which half.
                if let PendingFetch::Warm {
                    ref message_ids, ..
                } = request
                {
                    crate::bodylog!(
                        "warm  START {} bodies account={}",
                        message_ids.len(),
                        account_id.0
                    );
                    let results = provider.warm_many_bodies(&mut core, message_ids, &bodies_dir);
                    let warmed = results.iter().filter(|(_, ok)| *ok).count();
                    crate::bodylog!(
                        "warm  DONE  {warmed}/{} bodies in {}ms",
                        results.len(),
                        started.elapsed().as_millis()
                    );
                    for (message_id, ok) in results {
                        events(SyncEvent::BodyReady { message_id, ok });
                    }
                    continue;
                }
                let what = match &request {
                    PendingFetch::Body { message_id, .. } => format!("body {}", message_id.0),
                    PendingFetch::Attachment { attachment_id, .. } => {
                        format!("attachment {}", attachment_id.0)
                    }
                    PendingFetch::Warm { .. } => unreachable!("handled above"),
                };
                crate::bodylog!("fetch START {what} account={}", account_id.0);
                let ok = match &request {
                    PendingFetch::Body { message_id, .. } => provider
                        .open_one_body(&mut core, message_id, &bodies_dir)
                        .is_ok(),
                    PendingFetch::Attachment { attachment_id, .. } => provider
                        .download_one_attachment(
                            &mut core,
                            account_id,
                            attachment_id,
                            &attachments_dir,
                        )
                        .is_ok(),
                    PendingFetch::Warm { .. } => unreachable!("handled above"),
                };
                crate::bodylog!(
                    "fetch DONE  {what} ok={ok} in {}ms",
                    started.elapsed().as_millis()
                );
                request.emit_ready(events, ok);
                continue;
            }
            ConnectedFetch::None => {}
        }

        // A forced refresh is user-initiated work. Once foreground body or
        // attachment fetches have been served, hand the single live socket
        // to the mailbox at the head of that queue instead of letting the
        // currently-connected account enter another routine IDLE round.
        let current_id = connected
            .as_ref()
            .expect("connected was just ensured Some above")
            .0
            .clone();
        if next_refresh_account(&sources).is_some_and(|wanted| wanted != current_id) {
            connected = None;
            continue;
        }

        let (account_id, provider) = connected
            .as_mut()
            .expect("connected was just ensured Some above");

        // Never plain `tick`: this provider speaks for `account_id` only.
        match core.tick_for_account(account_id, &mut **provider) {
            Ok(TickOutcome::Idle) => {
                // `tick_for_account` said the operation queue is empty for
                // this account -- but new mail never arrives as a queued
                // operation (see this module's T-078 (b) doc comment
                // above), so before treating this as idle, give this
                // account's folders one chance at an inbound sync pass.
                //
                // T-090: `next_sync`'s external inputs are captured fresh
                // on every pass, not once -- the user can switch folders
                // or unplug the laptop at any moment, and a stale copy
                // would schedule against a screen state that no longer
                // holds. See `ScheduleSnapshot::capture` for the exact
                // sourcing and the poisoned-lock fallback.
                let schedule = ScheduleSnapshot::capture(clock.now(), &sources);
                let forced = next_refresh_account(&sources).as_ref() == Some(account_id);
                let step = if forced {
                    sync_forced_folder(
                        &core,
                        account_id,
                        &mut **provider,
                        &schedule,
                        &rx,
                        events,
                        &pending_fetches,
                    )
                } else {
                    sync_one_due_folder(
                        &core,
                        account_id,
                        &mut **provider,
                        &schedule,
                        &rx,
                        events,
                        &pending_fetches,
                    )
                };
                if forced {
                    finish_refresh_account(&sources, account_id);
                }
                match step {
                    FolderSyncStep::Shutdown => return,
                    FolderSyncStep::Attempted { drop_session } => {
                        // Exactly one folder was synced this turn, ok or
                        // not -- `Core::record_sync_attempt` already ran
                        // either way (see `sync_one_due_folder`). Treat
                        // like `Acked`/`Failed` above: something happened,
                        // so loop again immediately rather than counting
                        // this account as idle. `known_delay` is left
                        // alone -- a queue retry due on this same account
                        // is a separate timer and must still be honored on
                        // the next tick.
                        idle_streak = 0;
                        idle_wait = MAX_IDLE_POLL_SECS;
                        // T-091: the pass hit `SyncError::Auth` or
                        // `SyncError::Session` -- see `should_drop_session`
                        // for exactly which and why (`Store` deliberately
                        // does not set `drop_session`). Either way keeping
                        // the cached session around would just fail the
                        // exact same way, silently, forever (the bug this
                        // ticket exists to fix). Dropping it here, same
                        // as `ConnectionLost`/the local-storage-hiccup arm
                        // below, sends the very next pass through
                        // `factory.connect()` -- the same bounded
                        // reconnect-and-reauthorize path `apply` already
                        // uses (`connect_with_bounded_reauth`), never a
                        // second, competing reauth mechanism grown here.
                        if drop_session {
                            connected = None;
                        }
                    }
                    FolderSyncStep::Idle { hint } => {
                        // Neither the queue nor the folder scheduler has
                        // anything due right now for this account.
                        let retry_delay = known_delay.take();
                        let snooze_due = core
                            .next_snooze_deadline(Some(account_id))
                            .ok()
                            .flatten()
                            .map(|deadline| (deadline - schedule.now).max(0));
                        let wait = [retry_delay, hint, snooze_due, Some(MAX_IDLE_POLL_SECS)]
                            .into_iter()
                            .flatten()
                            .min()
                            .unwrap_or(MAX_IDLE_POLL_SECS);
                        if retry_delay.is_some() {
                            // A queue-apply retry timer is pending
                            // (T-089): do *not* go through
                            // `watch_inbox_for_push` -- a D32 backoff
                            // floor as short as two seconds is not
                            // worth `IDLE`'s round trips, and that
                            // exact timing is
                            // `retry_backs_off_and_does_not_hot_loop`'s
                            // job to keep proving, unperturbed.
                            //
                            // With more than one account the retry
                            // still ends this turn: another account
                            // may have work due right now, and it
                            // needs a different session. Only once
                            // every account in turn has come back
                            // idle is there actually nothing to do,
                            // and only then sleep -- for the shortest
                            // wait any of them asked for, so one
                            // account's 15-minute idle poll cannot
                            // bury another's 2-second retry.
                            if account_count > 1 {
                                connected = None;
                                idle_wait = idle_wait.min(wait);
                                idle_streak += 1;
                                if idle_streak >= account_count {
                                    idle_streak = 0;
                                    let secs =
                                        std::mem::replace(&mut idle_wait, MAX_IDLE_POLL_SECS);
                                    if wait_for_shutdown(&rx, &clock, secs, &pending_fetches) {
                                        return;
                                    }
                                }
                            } else if wait_for_shutdown(&rx, &clock, wait, &pending_fetches) {
                                return;
                            }
                        } else {
                            // No queue retry pending (T-089): watch
                            // the focused-or-Inbox folder for a server
                            // push instead of sleeping out `wait`.
                            // With more than one account the ceiling
                            // is sliced so this socket cannot park
                            // the one worker thread for minutes.
                            let idle_ceiling = if account_count > 1 {
                                wait.min(MULTI_ACCOUNT_IDLE_SECS)
                            } else {
                                wait
                            };
                            match watch_inbox_for_push(
                                &core,
                                account_id,
                                &mut **provider,
                                schedule.view.open_folder.as_deref(),
                                idle_ceiling,
                                &clock,
                                &rx,
                                events,
                                &pending_fetches,
                            ) {
                                InboxWatchStep::Shutdown => return,
                                InboxWatchStep::Synced { drop_session } => {
                                    // Something changed and was just
                                    // synced -- loop immediately, same
                                    // treatment as `Attempted` above.
                                    idle_streak = 0;
                                    idle_wait = MAX_IDLE_POLL_SECS;
                                    // T-091: same reasoning as
                                    // `FolderSyncStep::Attempted`'s own
                                    // `drop_session` arm above -- a push-
                                    // triggered pass can hit
                                    // `SyncError::Auth`/`SyncError::Session`
                                    // exactly like a scheduled one can.
                                    if drop_session {
                                        connected = None;
                                    }
                                }
                                InboxWatchStep::Yielded => {
                                    // Nothing to report this round.
                                    // Single account: loop immediately
                                    // and `IDLE` again. More than one:
                                    // drop the session so the next
                                    // account can take the one worker
                                    // thread -- the slice *was* the
                                    // wait, no extra sleep on top.
                                    if account_count > 1 {
                                        connected = None;
                                        idle_streak = 0;
                                        idle_wait = MAX_IDLE_POLL_SECS;
                                    }
                                }
                                InboxWatchStep::NoInboxFolder => {
                                    // No watchable folder yet (no
                                    // Inbox, no resolvable focus).
                                    // Reconnecting would cost a real
                                    // network round trip for no
                                    // reason. Single account: keep the
                                    // live session and fall back to
                                    // the bounded wait. More than one:
                                    // end the turn the same way a
                                    // retry-timer idle does.
                                    if account_count > 1 {
                                        connected = None;
                                        idle_wait = idle_wait.min(wait);
                                        idle_streak += 1;
                                        if idle_streak >= account_count {
                                            idle_streak = 0;
                                            let secs = std::mem::replace(
                                                &mut idle_wait,
                                                MAX_IDLE_POLL_SECS,
                                            );
                                            if wait_for_shutdown(
                                                &rx,
                                                &clock,
                                                secs,
                                                &pending_fetches,
                                            ) {
                                                return;
                                            }
                                        }
                                    } else if wait_for_shutdown(&rx, &clock, wait, &pending_fetches)
                                    {
                                        return;
                                    }
                                }
                                InboxWatchStep::ConnectionLost { err } => {
                                    // The session broke mid-watch (RFC
                                    // 2177: an idling connection can
                                    // simply drop). Not a second
                                    // reconnect mechanism -- fold it into
                                    // the exact same per-account
                                    // `connect_backoff`/D32 bookkeeping an
                                    // ordinary `ProviderFactory::connect`
                                    // failure already uses, a few dozen
                                    // lines above.
                                    let now = clock.now();
                                    let entry = connect_backoff
                                        .entry(account_id.0.clone())
                                        .or_insert((0, now));
                                    entry.0 += 1;
                                    let delay =
                                        feathermail_sync::backoff::backoff_delay_secs(entry.0);
                                    entry.1 = now + delay;
                                    let failed_account = account_id.clone();
                                    connected = None;
                                    events(SyncEvent::ConnectFailed {
                                        account_id: failed_account,
                                        message: connect_error_message(&err),
                                    });
                                    // Do not wait here, same reasoning as
                                    // the ordinary connect-failure arm
                                    // above: this account's own backoff
                                    // (or another account, once one
                                    // exists) is picked up on the very
                                    // next pass through the loop.
                                }
                            }
                        }
                    }
                }
            }
            Ok(TickOutcome::Acked(operation_id)) => {
                events(SyncEvent::Acked { operation_id });
                known_delay = None;
                idle_streak = 0;
                idle_wait = MAX_IDLE_POLL_SECS;
                // Loop again immediately: more of this account's queue may
                // already be due, and checking is a single indexed SELECT.
            }
            Ok(TickOutcome::Retry { delay, .. }) => {
                // Keep the session; do not wait here. The very next tick
                // (immediately below, on the next loop iteration) claims
                // nothing else is due yet -- `next_attempt_at` was just set
                // to `now + delay` -- and comes back `Idle`, which is the
                // branch that actually performs the bounded wait using
                // `known_delay`, on this same connection. This costs
                // exactly one cheap extra SELECT per failure, not a busy
                // loop and not a reconnect.
                known_delay = Some(delay);
            }
            Ok(TickOutcome::Failed { id, error }) => {
                events(SyncEvent::Failed {
                    operation_id: id,
                    error,
                });
                idle_streak = 0;
                idle_wait = MAX_IDLE_POLL_SECS;
                // Loop again immediately, same reasoning as `Acked`.
            }
            Err(_core_err) => {
                // A local storage hiccup mid-tick, not something
                // `ApplyError` has a variant for (see
                // `feathermail_core::locator`'s `sql_err_apply` doc comment
                // for the same shape of problem on the locator side). Drop
                // the connection and fall back to the bounded idle wait so
                // this cannot spin against a wedged database.
                connected = None;
                if wait_for_shutdown(&rx, &clock, MAX_IDLE_POLL_SECS, &pending_fetches) {
                    return;
                }
            }
        }
    }
}

/// A newly saved account has one local `Inbox` row so the shell can render
/// immediately, but that row intentionally has no IMAP mailbox name until a
/// server `LIST` is reconciled. Run that discovery before scheduling the
/// first inbound sync; otherwise `remote_folder` cannot resolve the
/// placeholder and the worker records a no-op failure forever without ever
/// reaching the server (T-077 bootstrap).
///
/// This is deliberately limited to an account with no resolvable Inbox.
/// Subsequent reconnects reuse the durable folder map instead of issuing a
/// `LIST` before every ordinary sync pass.
fn bootstrap_folders_if_needed(
    core: &mut Core,
    account_id: &AccountId,
    session: &mut dyn MailSession,
) -> Result<(), ConnectError> {
    let needs_discovery = core
        .folder_sync_inputs(account_id)
        .map_err(|err| ConnectError::invalid(err.message))?
        .iter()
        .filter(|folder| folder.role == feathermail_sync::schedule::FolderRole::Inbox)
        .all(|folder| core.remote_folder(account_id, &folder.id).is_err());
    if !needs_discovery {
        return Ok(());
    }

    let discovered = session.discover_folders()?;
    if discovered.is_empty() {
        // The default on narrow test doubles is intentionally a no-op. A
        // real IMAP server always exposes INBOX, so production never takes
        // this branch after a successful LIST.
        return Ok(());
    }
    core.sync_folders(account_id, &discovered)
        .map_err(|err| ConnectError::invalid(err.message))
}

/// What happened when [`run`]'s main loop checked, on an otherwise-idle
/// account, whether any of its folders is due for an inbound sync pass
/// (T-078 (b), see this module's own top-level doc comment).
///
/// `PartialEq`/`Debug` exist for T-090's scheduling tests, which compare
/// an `Idle { hint }` outcome against the exact second count
/// `next_sync` should have produced under a given focus/power input.
#[derive(Debug, PartialEq, Eq)]
enum FolderSyncStep {
    /// A [`WorkerCommand::Shutdown`] arrived while a sync pass was in
    /// progress. [`feathermail_sync::sync_folder`]'s own cancellation check (D11: an
    /// uninterruptible walk over a large mailbox must not make window
    /// close hang) already stopped it cleanly, and whatever progress it
    /// made was already durably saved. The caller must stop immediately --
    /// see [`sync_one_due_folder`]'s doc comment for why the `Shutdown`
    /// command itself was already consumed right here, and will never be
    /// seen again by a later `wait_for_shutdown` call.
    Shutdown,
    /// Exactly one folder was synced this call, successfully or not --
    /// [`Core::record_sync_attempt`] already ran either way. Same
    /// treatment as `TickOutcome::Acked`/`Failed`: something happened, so
    /// the caller should loop again immediately rather than counting this
    /// as an idle pass. `drop_session` is [`FolderSyncOutcome::drop_session`]
    /// carried through (T-091): `true` exactly when [`should_drop_session`]
    /// says so ([`feathermail_sync::SyncError::Auth`] or `::Session`),
    /// telling the caller the cached session is no good any more and must
    /// not be reused.
    Attempted { drop_session: bool },
    /// Nothing is due right now. `hint` is [`next_sync`]'s own best guess
    /// at how many seconds until something is, when it has one -- `None`
    /// only when this account currently has no folders to schedule
    /// against at all (e.g. a brand-new account before its first
    /// `Core::sync_folders` walk).
    Idle { hint: Option<i64> },
}

/// Runs one server pass selected by the user rather than by the cadence
/// scheduler. The focused real folder wins; overlays and All Accounts fall
/// back to this account's Inbox. The command remains bounded to one folder
/// per account so refreshing a unified view cannot monopolize the worker.
fn sync_forced_folder(
    core: &Core,
    account_id: &AccountId,
    session: &mut dyn MailSession,
    schedule: &ScheduleSnapshot,
    rx: &Receiver<WorkerCommand>,
    events: &(dyn Fn(SyncEvent) + Send),
    pending_fetches: &RefCell<VecDeque<PendingFetch>>,
) -> FolderSyncStep {
    let folders = core.folder_sync_inputs(account_id).unwrap_or_default();
    let Some(folder) = idle_watch_folder(&folders, schedule.view.open_folder.as_deref()) else {
        return FolderSyncStep::Idle { hint: None };
    };

    let shutdown_seen = AtomicBool::new(false);
    let is_cancelled = || {
        absorb_commands(rx, pending_fetches, &shutdown_seen);
        sync_pass_should_yield(pending_fetches, &shutdown_seen)
    };
    let outcome = run_one_folder_sync(
        core,
        account_id,
        session,
        &folder.id,
        schedule.now,
        &is_cancelled,
        events,
    );
    if shutdown_seen.load(Ordering::SeqCst) {
        FolderSyncStep::Shutdown
    } else {
        if outcome.ok {
            queue_warmup_for_folder(core, account_id, &folder.id, pending_fetches);
        }
        FolderSyncStep::Attempted {
            drop_session: outcome.drop_session,
        }
    }
}

/// Checks whether one of `account_id`'s folders is due for an inbound
/// sync pass right now and, if so, runs it -- at most one
/// [`feathermail_sync::sync_folder`] call per invocation, never a whole account's worth of
/// folders at once. An account with many folders must not be able to hold
/// up every other account's queued operations for that many sync passes
/// in a row -- the identical no-starvation reasoning already applied to
/// the operation queue itself (this module's top doc comment).
///
/// # Why `is_cancelled` must consume `Shutdown` off `rx`, not just peek it
///
/// [`feathermail_sync::sync_folder`] takes `is_cancelled: &dyn Fn() -> bool`, polled between
/// batches of work. The only way to answer that without blocking the sync
/// pass until some *future* command arrives is a non-blocking `try_recv`
/// against this same `rx` -- the same channel every other wait in this
/// module reads from -- and `mpsc::Receiver` has no "peek" that would
/// leave a message sitting for a later reader. So a `Shutdown` sent while
/// a sync pass is running is drained right here. If this function did not
/// tell its caller that happened, [`run`]'s later `wait_for_shutdown`
/// calls would sit waiting for a *second* `Shutdown` that is never
/// coming -- the worker would never exit. [`FolderSyncStep::Shutdown`] is
/// that signal, and [`run`] must return as soon as it sees it.
///
/// T-118: a click (`FetchBody` / `FetchAttachment`) must yield the pass
/// the same way `Shutdown` does. Header backfill of a large mailbox is
/// minutes of `UID FETCH` batches on the one live socket; queuing the
/// click and running the rest of the folder is the letter that never
/// loads in All accounts. Progress already saved between batches is kept
/// (`sync_folder` returns `Ok` with `cancelled: true`). A warm-up is
/// not a person waiting and does not yield.
fn sync_one_due_folder(
    core: &Core,
    account_id: &AccountId,
    session: &mut dyn MailSession,
    // T-090: one consistent read of `next_sync`'s external inputs (`now`,
    // `Focus`, `PowerState`), captured by the caller (`run`) from the
    // shell's `Viewport` report and the system `PowerProbe` -- taken as a
    // parameter, rather than read off those sources here, so the
    // scheduling decision stays directly testable without a worker
    // thread, a channel, or sysfs (see this module's own T-090 tests).
    schedule: &ScheduleSnapshot,
    rx: &Receiver<WorkerCommand>,
    events: &(dyn Fn(SyncEvent) + Send),
    pending_fetches: &RefCell<VecDeque<PendingFetch>>,
) -> FolderSyncStep {
    let folders = core.folder_sync_inputs(account_id).unwrap_or_default();
    if folders.is_empty() {
        return FolderSyncStep::Idle { hint: None };
    }

    let folder_id = match next_sync(&folders, &schedule.focus(), &schedule.power, schedule.now) {
        Decision::Nothing => return FolderSyncStep::Idle { hint: None },
        Decision::Next { folder, in_secs } => {
            if in_secs > 0 {
                return FolderSyncStep::Idle {
                    hint: Some(in_secs),
                };
            }
            folder
        }
    };

    let shutdown_seen = AtomicBool::new(false);
    // T-080: a `FetchBody` sent while this sync pass is in progress must
    // not be silently dropped -- `try_recv` is destructive, and this is
    // the only place reading `rx` until the pass finishes (see this
    // function's own doc comment on why `Shutdown` has to be drained the
    // same way). Routed into `pending_fetches` instead so `run` still
    // sees it once this call returns, exactly as if it had arrived a
    // moment earlier or later.
    // T-118: and a foreground fetch *yields* the pass. IDLE already
    // broke the round on FetchBody; a first-time backfill of tens of
    // thousands of headers is the same occupancy of the one socket,
    // just without a 30s ceiling.
    let is_cancelled = || {
        absorb_commands(rx, pending_fetches, &shutdown_seen);
        sync_pass_should_yield(pending_fetches, &shutdown_seen)
    };

    let outcome = run_one_folder_sync(
        core,
        account_id,
        session,
        &folder_id,
        schedule.now,
        &is_cancelled,
        events,
    );

    if shutdown_seen.load(Ordering::SeqCst) {
        FolderSyncStep::Shutdown
    } else {
        if outcome.ok {
            queue_warmup_for_folder(core, account_id, &folder_id, pending_fetches);
        }
        FolderSyncStep::Attempted {
            drop_session: outcome.drop_session,
        }
    }
}

/// Runs exactly one [`MailSession::sync_one_folder`] pass against
/// `folder_id` and records the attempt: the tail
/// [`sync_one_due_folder`]'s scheduled pass and
/// [`watch_inbox_for_push`]'s push-triggered pass (T-089) share, so the
/// two paths cannot drift on what "ran a sync pass" means -- resolving
/// `folder_id` to its `remote_name` (or recording a failed attempt if it
/// has none yet, T-077), calling [`Core::record_sync_attempt`] either way
/// (so [`next_sync`]'s no-starvation contract holds even for an
/// unresolvable folder -- see the doc comment this replaced, just below,
/// for the fuller argument), and emitting exactly one
/// [`SyncEvent::FolderSynced`].
fn run_one_folder_sync(
    core: &Core,
    account_id: &AccountId,
    session: &mut dyn MailSession,
    folder_id: &str,
    now: i64,
    is_cancelled: &dyn Fn() -> bool,
    events: &(dyn Fn(SyncEvent) + Send),
) -> FolderSyncOutcome {
    // Invariant this function must keep, and the one
    // `folder_synced_event_carries_a_reason_exactly_when_it_failed` exists
    // to enforce: `error` on the emitted `SyncEvent::FolderSynced` is
    // `Some` exactly when `ok` is `false`, never the reverse and never
    // both-or-neither. That is why every arm below produces `(ok, reason)`
    // together rather than deriving one from the other after the fact.
    events(SyncEvent::FolderSyncStarted {
        account_id: account_id.clone(),
        folder_id: folder_id.to_string(),
    });
    let (ok, reason, drop_session) = match core.remote_folder(account_id, folder_id) {
        Ok(remote_name) => {
            let mut store = core.sync_store(account_id, folder_id);
            match session.sync_one_folder(&mut store, &remote_name, now, is_cancelled) {
                Ok(_) => (true, None, false),
                Err(err) => (
                    false,
                    Some(sync_failure_reason(&err)),
                    should_drop_session(&err),
                ),
            }
        }
        // No `remote_id` yet for this local folder (T-077's placeholder
        // case: never matched against a server `LIST`, or a system key
        // that has no configured destination). Nothing resolvable to
        // `SELECT` on the wire. Recorded as a failed attempt anyway --
        // otherwise `next_sync` would report this same folder due again
        // on every single pass forever (its own doc comment's
        // no-starvation contract: an attempt must always be recorded once
        // it says "now", success or not). `Other`, not `Auth`, and no
        // session drop: nothing here ever touched a session, so there is
        // nothing to drop.
        Err(_) => (false, Some(SyncFailureReason::Other), false),
    };
    let _ = core.record_sync_attempt(account_id, folder_id, ok, now);
    events(SyncEvent::FolderSynced {
        account_id: account_id.clone(),
        folder_id: folder_id.to_string(),
        ok,
        error: reason,
    });
    FolderSyncOutcome {
        drop_session,
        // T-119: a successful header pass is the moment new mail exists
        // as rows without bodies. Callers enqueue a warm-up from this,
        // not from the event -- the event has already been fired, and
        // the live session is still in hand.
        ok,
    }
}

/// What [`run_one_folder_sync`] found, for its two callers
/// ([`sync_one_due_folder`], [`watch_inbox_for_push`]) to fold into their
/// own step enums (T-091).
struct FolderSyncOutcome {
    /// `true` when [`should_drop_session`] says the failure means the
    /// cached *session*, not just this one folder's attempt, is no good
    /// any more -- so `run` must not keep using it. See
    /// `FolderSyncStep::Attempted`'s and `InboxWatchStep::Synced`'s own
    /// doc comments for where this is acted on.
    drop_session: bool,
    /// Whether the pass itself succeeded. A failed pass does not enqueue
    /// a warm-up: there is nothing new on disk to fetch, and the socket
    /// may already be the one [`should_drop_session`] wants thrown away.
    ok: bool,
}

/// Sorts a [`feathermail_sync::SyncError`] into the two buckets a
/// [`SyncEvent::FolderSynced`] is willing to say happened (T-091). D14,
/// the identical boundary [`connect_error_message`] enforces a few
/// functions down: the `String` inside `SyncError::Session`/`Store` is a
/// provider's own wire text (or a local storage error), never fixed human
/// copy, so it must never reach this worker's caller -- only the bare
/// *kind* of failure crosses, never the message.
fn sync_failure_reason(err: &SyncError) -> SyncFailureReason {
    match err {
        SyncError::Auth => SyncFailureReason::Auth,
        SyncError::Session(_) | SyncError::Store(_) => SyncFailureReason::Other,
    }
}

/// Whether a failed [`MailSession::sync_one_folder`] means the *cached
/// session*, not just this one folder's attempt, is no good any more
/// (T-091 (б), the half of this ticket that was still unfixed after the
/// first pass: `SyncFailureReason` alone cannot drive this decision,
/// because it deliberately folds `Session` and `Store` into the same
/// `Other` tag for the *event* -- that collapse is fine for a toast/
/// diagnostics line (neither variant's raw text is allowed past D14
/// either way), but it is not fine for deciding whether to reconnect,
/// which needs the finer distinction this function alone makes:
///
/// - [`SyncError::Auth`]: the session's authorization is gone. Obviously
///   worth dropping -- same as before this fix, and still unreachable
///   from a live IMAP server via the `sync` path today (see
///   `feathermail_providers::sync_session::map_err`'s own doc comment);
///   kept here for correctness and for whenever a provider adapter grows
///   a real protocol-level signal for it.
/// - [`SyncError::Session(_)`]: the conversation on the wire broke --
///   reset connection, unexpected response, a TLS error, the server
///   closing the socket. Keeping this session cached is pointless: the
///   underlying socket is very likely dead, and every following pass
///   would just fail against the same corpse. Reconnecting costs one
///   TCP+TLS handshake; if the server is genuinely unreachable, the
///   `connect()` that follows fails on its own and falls under the D32
///   backoff already applied to failed connects, so this cannot turn
///   into a hot loop (see
///   `repeated_sync_error_auth_failures_reconnect_exactly_once_per_failure_not_a_hot_loop`,
///   which proves the same shape for `Auth`).
/// - [`SyncError::Store(_)`]: the *local* database failed -- the socket,
///   if any, is untouched. Reconnecting fixes nothing here (the next
///   `sync_one_folder` call would hit the same broken SQLite state
///   through a brand-new session exactly as it would through the old
///   one) and would just add reconnect churn on every transient local
///   hiccup for no benefit. Left `false`.
fn should_drop_session(err: &SyncError) -> bool {
    match err {
        SyncError::Auth | SyncError::Session(_) => true,
        SyncError::Store(_) => false,
    }
}

/// What [`watch_inbox_for_push`] found (T-089). See that function's doc
/// comment, and this module's own T-089 doc comment, for the full design.
enum InboxWatchStep {
    /// A [`WorkerCommand::Shutdown`] arrived while watching. Already
    /// consumed off `rx` here for the same reason
    /// [`sync_one_due_folder`]'s `is_cancelled` consumes it (see that
    /// function's doc comment): `run` must return immediately, without a
    /// second `wait_for_shutdown` call that would then wait for a second
    /// `Shutdown` that is never coming.
    Shutdown,
    /// This account has nothing [`idle_watch_folder`] can watch that
    /// [`Core::remote_folder`] can actually `SELECT`: no Inbox yet (a
    /// brand-new account before its first `Core::sync_folders` walk),
    /// no matching focused folder, or neither has a `remote_id`
    /// resolved yet (T-077). Caller falls back to its old plain wait.
    NoInboxFolder,
    /// The live session broke while selecting the mailbox, querying
    /// capabilities, or during the IDLE/`NOOP` round itself -- RFC 2177
    /// is explicit that a server may drop an idling connection, so this
    /// is expected, not exceptional. Not a second reconnect mechanism:
    /// the caller folds this into the exact same per-account
    /// `connect_backoff` bookkeeping (D32) an ordinary
    /// `ProviderFactory::connect` failure already uses, a few dozen lines
    /// above in `run`.
    ConnectionLost { err: ConnectError },
    /// One round ended with nothing to report -- either the ceiling this
    /// call was given was reached with the server silent, or
    /// `should_stop` broke the round early because a
    /// `Wake`/`FetchBody`/`Shutdown` command arrived (any `FetchBody` seen
    /// is already pushed into `pending_fetches`, exactly like
    /// `sync_one_due_folder`'s `is_cancelled`). Either way, `run` should
    /// loop immediately and let its normal per-pass logic
    /// (`tick_for_account`, `sync_one_due_folder`) decide what happens
    /// next -- there is nothing IDLE-specific left to do here.
    Yielded,
    /// The server reported a change on the watched mailbox and it was
    /// synced immediately as a result ([`run_one_folder_sync`] already
    /// ran, so `Core::record_sync_attempt` and `SyncEvent::FolderSynced`
    /// are already done) -- same contract as `FolderSyncStep::Attempted`,
    /// `drop_session` included (T-091).
    Synced { drop_session: bool },
}

/// Which folder of `folders` this account should hold IMAP `IDLE` on
/// (T-089 remainder).
///
/// `open_folder` is the shell's [`Viewport::open_folder`] -- a globally
/// unique `folders.id` (`{account}:{slug}`). If it names one of *this*
/// account's folders, that one wins: it is the mailbox the user is
/// looking at, which is T-089's artifact. Otherwise the account's
/// Inbox: new mail still arrives there when the user is in Settings,
/// looking at another account, or on an overlay (`starred`/`snoozed`)
/// that matches no [`FolderInput`]. `None` only when this account has
/// neither a matching focus nor an Inbox -- a brand-new account
/// before its first `Core::sync_folders` walk.
fn idle_watch_folder<'a>(
    folders: &'a [FolderInput],
    open_folder: Option<&str>,
) -> Option<&'a FolderInput> {
    if let Some(open) = open_folder {
        if let Some(focused) = folders.iter().find(|f| f.id == open) {
            return Some(focused);
        }
    }
    folders.iter().find(|f| f.role == FolderRole::Inbox)
}

/// Resolves [`idle_watch_folder`]'s pick to a mailbox the session can
/// actually `SELECT`. A focused folder with no `remote_id` yet (T-077)
/// falls back to Inbox rather than giving up on push altogether; if
/// Inbox is equally unresolvable there is nothing to watch.
fn resolve_idle_watch(
    folders: &[FolderInput],
    core: &Core,
    account_id: &AccountId,
    open_folder: Option<&str>,
) -> Option<(String, String)> {
    let preferred = idle_watch_folder(folders, open_folder)?;
    if let Ok(name) = core.remote_folder(account_id, &preferred.id) {
        return Some((preferred.id.clone(), name));
    }
    if preferred.role == FolderRole::Inbox {
        return None;
    }
    let inbox = folders.iter().find(|f| f.role == FolderRole::Inbox)?;
    let name = core.remote_folder(account_id, &inbox.id).ok()?;
    Some((inbox.id.clone(), name))
}

/// Watches `account_id`'s focused-or-Inbox folder for a server push
/// instead of the caller just sleeping (T-089): `IDLE`s (or, on a
/// server without `IDLE`, one honest `NOOP` poll, D30) for up to
/// `idle_timeout_secs`, and on a change, runs an actual sync pass
/// immediately via [`run_one_folder_sync`] against the mailbox that
/// was watched, not a hardcoded Inbox.
///
/// # Why command latency here is bounded by a poll slice, not instant
///
/// `should_stop` -- passed down to
/// [`MailSession::idle_once`]/`feathermail_providers::run_idle_with` -- is
/// only re-checked between poll slices (currently 5s,
/// `crate::provider_factory::IDLE_POLL_SLICE`) while the socket read that
/// backs one slice is blocked waiting on the server. A true
/// `mpsc::Receiver::recv_timeout` (what [`wait_for_shutdown`] uses) would
/// notice a command the instant it arrives, but there is no primitive here
/// that can block on *both* a TCP read and this channel at once without
/// either a second thread or a poll -- see this module's top-level T-089
/// doc comment for the fuller tradeoff and the alternatives considered.
/// A few seconds of extra latency on a `Wake`/`FetchBody`/`Shutdown`,
/// against a ceiling of up to 29 minutes, is the deliberate trade this
/// makes; it is never allowed to wait out the *whole* ceiling, which is
/// what the mutation tests for this function prove.
///
/// # Why *any* command breaks the round, not only `Shutdown`
///
/// A `Wake` means the UI just queued an operation for this very account
/// (e.g. mark-as-read); `tick_for_account` is what actually drains it, and
/// it is never called while this function is blocked in `IDLE`. If
/// `should_stop` only reacted to `Shutdown`, a `Wake` sent during a long
/// `IDLE` round would sit unconsumed until the round ends on its own --
/// exactly the "не уедет пометка" starvation this ticket calls out by
/// name. So `should_stop` here breaks the round on `Wake` and `FetchBody`
/// too, not just `Shutdown`; [`InboxWatchStep::Yielded`] is what tells
/// `run` "something interrupted this, go handle it", without this
/// function needing to know *what*.
#[allow(clippy::too_many_arguments)]
fn watch_inbox_for_push(
    core: &Core,
    account_id: &AccountId,
    session: &mut dyn MailSession,
    open_folder: Option<&str>,
    idle_timeout_secs: i64,
    clock: &impl WorkerClock,
    rx: &Receiver<WorkerCommand>,
    events: &(dyn Fn(SyncEvent) + Send),
    pending_fetches: &RefCell<VecDeque<PendingFetch>>,
) -> InboxWatchStep {
    let folders = core.folder_sync_inputs(account_id).unwrap_or_default();
    let Some((watched_id, remote_name)) =
        resolve_idle_watch(&folders, core, account_id, open_folder)
    else {
        return InboxWatchStep::NoInboxFolder;
    };

    let shutdown_seen = AtomicBool::new(false);
    let interrupted = AtomicBool::new(false);
    // See this function's own doc comment for why `Wake`/`FetchBody` stop
    // the round exactly like `Shutdown` does here. Header backfill now
    // yields on a foreground fetch too (T-118); IDLE still also yields
    // on `Wake` and warm-up, because parking the socket for up to 29
    // minutes with *any* command waiting is the original T-089 hole.
    let mut should_stop = || {
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                WorkerCommand::Shutdown => shutdown_seen.store(true, Ordering::SeqCst),
                WorkerCommand::Wake => interrupted.store(true, Ordering::SeqCst),
                WorkerCommand::FetchBody {
                    account_id,
                    message_id,
                } => {
                    pending_fetches.borrow_mut().push_back(PendingFetch::Body {
                        account_id,
                        message_id,
                    });
                    interrupted.store(true, Ordering::SeqCst);
                }
                WorkerCommand::WarmBodies {
                    account_id,
                    message_ids,
                } => {
                    pending_fetches.borrow_mut().push_back(PendingFetch::Warm {
                        account_id,
                        message_ids,
                    });
                    interrupted.store(true, Ordering::SeqCst);
                }
                WorkerCommand::FetchAttachment {
                    account_id,
                    attachment_id,
                } => {
                    pending_fetches
                        .borrow_mut()
                        .push_back(PendingFetch::Attachment {
                            account_id,
                            attachment_id,
                        });
                    interrupted.store(true, Ordering::SeqCst);
                }
            }
        }
        shutdown_seen.load(Ordering::SeqCst) || interrupted.load(Ordering::SeqCst)
    };

    let ceiling_secs = idle_timeout_secs.clamp(1, IDLE_TIMEOUT_SECS as i64) as u64;
    // Read before the round so the `TimedOut` arm below can tell how much
    // of its own ceiling the round actually spent (T-114).
    let started_at = clock.now();
    let round = session.idle_once(
        &remote_name,
        Duration::from_secs(ceiling_secs),
        &mut should_stop,
    );

    match round {
        Err(err) => InboxWatchStep::ConnectionLost { err },
        Ok(IdleRound {
            outcome: IdleOutcome::Events(_events),
            ..
        }) => {
            // Something changed on the watched mailbox: sync it right
            // now rather than waiting for `sync_one_due_folder`'s own
            // schedule to come back around to it. Reuses the same
            // `is_cancelled` shape `sync_one_due_folder` uses (drains
            // `rx` so a command sent *during* this sync pass is not
            // dropped either) rather than the `should_stop` closure
            // above, which already stopped being polled once
            // `idle_once` returned.
            let is_cancelled = || {
                absorb_commands(rx, pending_fetches, &shutdown_seen);
                sync_pass_should_yield(pending_fetches, &shutdown_seen)
            };
            // Read the clock *after* the round returns, not before it
            // started: `idle_once` can block for the length of a whole
            // `IDLE` round (up to 29 minutes, RFC 2177), so a `now` taken
            // before it would understamp `Core::record_sync_attempt` by
            // that entire span -- making a folder that was *just* synced
            // by this very push look overdue again on the very next pass.
            let now = clock.now();
            let outcome = run_one_folder_sync(
                core,
                account_id,
                session,
                &watched_id,
                now,
                &is_cancelled,
                events,
            );
            if shutdown_seen.load(Ordering::SeqCst) {
                InboxWatchStep::Shutdown
            } else {
                if outcome.ok {
                    queue_warmup_for_folder(core, account_id, &watched_id, pending_fetches);
                }
                InboxWatchStep::Synced {
                    drop_session: outcome.drop_session,
                }
            }
        }
        Ok(IdleRound {
            outcome: IdleOutcome::Stopped,
            ..
        }) => {
            // `should_stop` broke the round early because a
            // `Wake`/`FetchBody`/`Shutdown` command arrived (any
            // `FetchBody` seen is already pushed into `pending_fetches`).
            // Nothing IDLE-specific left to do -- `run` re-evaluates the
            // queue and folder schedule from scratch on its very next
            // pass. No pacing wait belongs here regardless of
            // `idle_capable`, unlike the no-IDLE `TimedOut` arm below:
            // this round did not run its own clock out on its own --
            // something already needs attention right now.
            if shutdown_seen.load(Ordering::SeqCst) {
                InboxWatchStep::Shutdown
            } else {
                InboxWatchStep::Yielded
            }
        }
        Ok(IdleRound {
            outcome: IdleOutcome::TimedOut,
            idle_capable: true,
        }) => {
            // Real IDLE ceiling reached (RFC 2177's ~29 minutes), server
            // silent. `run_idle_with` only returns `TimedOut` when its
            // own `should_stop` check, made in the same loop iteration,
            // was false, so `shutdown_seen` cannot be true here -- go
            // straight back into another IDLE wait.
            //
            // T-114: with `feathermail_providers::run_idle_with` the
            // round *is* the wait -- it returns `TimedOut` only once
            // `idle_timeout` of real time has passed -- so the subtraction
            // below comes out at zero or less and nothing else happens.
            // The guard is for the case where it does not: a server that
            // ends the IDLE the moment it starts, or any other
            // `MailSession` that answers `TimedOut` without having spent
            // the ceiling, would otherwise turn this arm into an
            // `IDLE`/`DONE` loop hammered as fast as the thread can run
            // it -- the same hazard the `idle_capable: false` arm below
            // already paces around, and the one
            // `ten_quiet_minutes_do_not_become_a_polling_storm` measures.
            // Paced through `wait_for_shutdown` like every other pause in
            // this module, so a `Shutdown`/`FetchBody` sent during it is
            // neither lost nor made to wait the pause out.
            // Clamped, not just floored at zero: `clock.now()` is wall time
            // (`SystemClock` reads the system clock), so an NTP step
            // backwards during the round would otherwise compute an
            // "unspent" longer than the ceiling itself and park the watch
            // for it.
            let spent = clock.now() - started_at;
            let unspent = (ceiling_secs as i64)
                .saturating_sub(spent)
                .clamp(0, ceiling_secs as i64);
            if unspent > 0 && wait_for_shutdown(rx, clock, unspent, pending_fetches) {
                InboxWatchStep::Shutdown
            } else {
                InboxWatchStep::Yielded
            }
        }
        Ok(IdleRound {
            outcome: IdleOutcome::TimedOut,
            idle_capable: false,
        }) => {
            // D30's no-IDLE fallback: the `idle_once` call above was a
            // single immediate `NOOP` round trip, not a wait (see
            // `feathermail_providers::run_idle_with`'s `!caps.idle`
            // branch). Looping straight back into another one the way
            // the real-IDLE arm above does would be
            // `SELECT`/`CAPABILITY`/`NOOP` hammered against a live
            // server in a tight loop -- exactly the class of bug this
            // ticket must not create (see `IdleRound`'s own doc comment,
            // which named this obligation before this arm read
            // `idle_capable` at all). Pace it by `NO_IDLE_POLL_SECS` the
            // same way the `NoInboxFolder`/retry-timer branches in `run`
            // already pace their own waits: through `wait_for_shutdown`,
            // so a `Shutdown`/`FetchBody` sent during the pause is not
            // lost and does not wait out the full minute (`recv_timeout`
            // underneath, never a bare `sleep`). No second wait
            // primitive introduced.
            //
            // Cap by this round's own ceiling: a multi-account slice
            // of [`MULTI_ACCOUNT_IDLE_SECS`] must not then sit in a
            // 60s pause that starves the other account, and a
            // single-account hint shorter than 60s should not be
            // stretched either. When the ceiling is the 15-minute
            // idle poll (the no-IDLE hammer test), this is still
            // exactly `NO_IDLE_POLL_SECS`.
            let pace = (NO_IDLE_POLL_SECS as i64).min(idle_timeout_secs.max(1));
            if wait_for_shutdown(rx, clock, pace, pending_fetches) {
                InboxWatchStep::Shutdown
            } else {
                InboxWatchStep::Yielded
            }
        }
    }
}

/// Picks which account to connect to next: round-robin over
/// `Core::list_accounts()`, so every account gets a turn and each one is
/// only ever handed its own operations ([`Core::tick_for_account`]).
/// Accounts still serving a connect backoff are skipped rather than
/// retried, so one unreachable mailbox cannot hold up the others or
/// produce a `ConnectFailed` event on every lap.
/// Also reports how many accounts there are, which is what tells the loop
/// when a sweep of per-account `Idle`s has actually covered all of them.
/// Returns `None` when there are no accounts at all yet (fresh profile,
/// Welcome screen still showing).
///
/// Takes `backoff` by `&mut` for one reason: this is the only place in the
/// loop that reads the profile's current account list, so it is also the
/// only place that can tell a backoff entry apart from an entry for an
/// account that no longer exists. An account removed while it was serving a
/// backoff (Settings -> Remove does not restart the worker) would otherwise
/// leave its entry behind forever, and `soonest_retry` would keep answering
/// "due now" for a mailbox nobody can connect to any more.
fn pick_account(
    core: &Core,
    cursor: &mut usize,
    count: &mut usize,
    backoff: &mut HashMap<String, (u32, i64)>,
    now: i64,
) -> Option<AccountId> {
    let accounts = core.list_accounts().ok()?;
    // Prune before the empty check, not after: "the last account was just
    // removed" is exactly the case that leaves an orphan behind.
    backoff.retain(|id, _| accounts.iter().any(|a| a.id.as_str() == id));
    if accounts.is_empty() {
        return None;
    }
    *count = accounts.len();
    // At most one full lap: every account gets looked at exactly once, so
    // this returns the next connectable one or `None`, never spins.
    for _ in 0..accounts.len() {
        let idx = *cursor % accounts.len();
        *cursor = cursor.wrapping_add(1);
        let id = &accounts[idx].id;
        let due = match backoff.get(id.as_str()) {
            Some((_, next_attempt_at)) => *next_attempt_at <= now,
            None => true,
        };
        if due {
            return Some(id.clone());
        }
    }
    None
}

/// Seconds until the earliest account still serving a connect backoff is
/// allowed to be tried again, or `None` if none is. `0` when one is
/// already due (the caller was told `None` for some other reason).
fn soonest_retry(backoff: &HashMap<String, (u32, i64)>, now: i64) -> Option<i64> {
    backoff
        .values()
        .map(|(_, next_attempt_at)| (next_attempt_at - now).max(0))
        .min()
}

/// Waits up to `timeout_secs`. Returns `true` if the caller should shut
/// down, `false` if it should loop again (a `Wake` arrived, or the
/// timeout simply elapsed -- both mean "try again").
///
/// T-080: `clock.wait` consumes at most one command off `rx`; if that one
/// command is a `FetchBody` and this function only checked it against
/// `Shutdown`, the fetch would vanish -- `recv_timeout` does not put it
/// back. So a `FetchBody` seen here is pushed into `pending_fetches`
/// before returning `false`, exactly like `sync_one_due_folder`'s
/// `is_cancelled` does for the same reason.
/// Drain every command currently sitting on `rx` into `pending_fetches`
/// (or the shutdown flag). `try_recv` is destructive, and this is the
/// only reader until the caller returns, so a `FetchBody` seen here
/// would otherwise vanish -- the same reason T-080 routed these into
/// the queue instead of dropping them.
fn absorb_commands(
    rx: &Receiver<WorkerCommand>,
    pending_fetches: &RefCell<VecDeque<PendingFetch>>,
    shutdown_seen: &AtomicBool,
) {
    while let Ok(cmd) = rx.try_recv() {
        match cmd {
            WorkerCommand::Shutdown => shutdown_seen.store(true, Ordering::SeqCst),
            WorkerCommand::FetchBody {
                account_id,
                message_id,
            } => {
                pending_fetches.borrow_mut().push_back(PendingFetch::Body {
                    account_id,
                    message_id,
                });
            }
            WorkerCommand::WarmBodies {
                account_id,
                message_ids,
            } => {
                pending_fetches.borrow_mut().push_back(PendingFetch::Warm {
                    account_id,
                    message_ids,
                });
            }
            WorkerCommand::FetchAttachment {
                account_id,
                attachment_id,
            } => {
                pending_fetches
                    .borrow_mut()
                    .push_back(PendingFetch::Attachment {
                        account_id,
                        attachment_id,
                    });
            }
            WorkerCommand::Wake => {}
        }
    }
}

/// T-118: a header-sync pass yields for `Shutdown` and for a person
/// waiting on a body or an attachment. A warm-up stays queued and the
/// pass continues -- filling a cache nobody asked for must not kick a
/// first-time Inbox backfill off the socket every time the list paints.
fn sync_pass_should_yield(
    pending_fetches: &RefCell<VecDeque<PendingFetch>>,
    shutdown_seen: &AtomicBool,
) -> bool {
    shutdown_seen.load(Ordering::SeqCst)
        || pending_fetches
            .borrow()
            .iter()
            .any(PendingFetch::is_foreground)
}

/// After a successful header pass, queue the newest bodies of that folder
/// so a just-arrived letter is on disk before anyone clicks it (D17, T-119).
///
/// The shell's warm-up used to be the only door, and it skipped while a
/// previous run was still draining -- new mail sat as headers until a
/// click. The worker already has the live session, so this is the same
/// socket and the same turn, not a later GTK command that can miss IDLE.
fn queue_warmup_for_folder(
    core: &Core,
    account_id: &AccountId,
    folder_id: &str,
    pending_fetches: &RefCell<VecDeque<PendingFetch>>,
) {
    let needed = core
        .messages_needing_warmup(&FolderId(folder_id.to_string()), PREFETCH_BODIES)
        .unwrap_or_default();
    enqueue_fresh_warmup(&mut pending_fetches.borrow_mut(), account_id, needed);
}

/// Push a warm-up for ids that are not already queued, newest first.
///
/// A click still outranks this (`is_foreground`): T-118 yielded the
/// header pass so the click can run next, and queuing a warm-up in front
/// of it would put the person back behind a cache fill. The cap is
/// [`PREFETCH_CHUNK`] so a click that lands while the fetch is on the
/// wire waits for one chunk, not a hundred (T-024). The rest of a cold
/// folder stays the shell's job.
fn enqueue_fresh_warmup(
    pending: &mut VecDeque<PendingFetch>,
    account_id: &AccountId,
    needed: Vec<MessageId>,
) {
    if pending.iter().any(PendingFetch::is_foreground) {
        return;
    }
    let mut already = HashSet::new();
    for request in pending.iter() {
        if let PendingFetch::Warm {
            account_id: queued,
            message_ids,
        } = request
        {
            if queued == account_id {
                already.extend(message_ids.iter().cloned());
            }
        }
    }
    let fresh: Vec<MessageId> = needed
        .into_iter()
        .filter(|id| !already.contains(id))
        .take(PREFETCH_CHUNK)
        .collect();
    if fresh.is_empty() {
        return;
    }
    pending.push_front(PendingFetch::Warm {
        account_id: account_id.clone(),
        message_ids: fresh,
    });
}

fn wait_for_shutdown(
    rx: &Receiver<WorkerCommand>,
    clock: &impl WorkerClock,
    timeout_secs: i64,
    pending_fetches: &RefCell<VecDeque<PendingFetch>>,
) -> bool {
    match clock.wait(rx, timeout_secs.max(0)) {
        Some(WorkerCommand::Shutdown) => true,
        Some(WorkerCommand::FetchBody {
            account_id,
            message_id,
        }) => {
            pending_fetches.borrow_mut().push_back(PendingFetch::Body {
                account_id,
                message_id,
            });
            false
        }
        Some(WorkerCommand::WarmBodies {
            account_id,
            message_ids,
        }) => {
            pending_fetches.borrow_mut().push_back(PendingFetch::Warm {
                account_id,
                message_ids,
            });
            false
        }
        Some(WorkerCommand::FetchAttachment {
            account_id,
            attachment_id,
        }) => {
            pending_fetches
                .borrow_mut()
                .push_back(PendingFetch::Attachment {
                    account_id,
                    attachment_id,
                });
            false
        }
        _ => false,
    }
}

/// D14: only `ConnectError`'s human `message` ever leaves this module in a
/// [`SyncEvent`] -- never `details`. `details` on the `Auth`/`Network`
/// variants can carry close-to-raw protocol/server text (see
/// `crates/providers/src/wire.rs`'s `sanitize`, which only strips control
/// characters, not secrets) and existing tests only prove specific known
/// servers don't echo the password back in it -- not that no server ever
/// could. `message` is the one field every existing `ConnectError`
/// constructor (`ConnectError::auth`/`reauth`/`network`/`invalid`) sets to
/// fixed, human, non-secret text (TZ §103/D46), so it's the only field
/// this worker is willing to forward.
fn connect_error_message(err: &ConnectError) -> String {
    match err {
        ConnectError::Auth { message, .. }
        | ConnectError::Network { message, .. }
        | ConnectError::Invalid { message, .. } => message.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use feathermail_core::remote::DiscoveredFolder;
    use feathermail_core::{
        Command, CoreError, CoreSyncStore, ErrorCode, FolderKind, MailProvider, Operation, ThreadId,
    };
    use feathermail_sync::{SyncError, SyncOutcome, SyncStore};
    use std::path::Path;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Instant;

    // --- test-only MailProvider/ProviderFactory doubles: no network, no keyring. ---

    struct FakeProvider {
        fail_network: Arc<AtomicU32>,
        applies: Arc<AtomicU32>,
    }

    impl MailProvider for FakeProvider {
        fn apply(&mut self, _op: &Operation) -> Result<(), ApplyError> {
            self.applies.fetch_add(1, Ordering::SeqCst);
            let remaining = self.fail_network.load(Ordering::SeqCst);
            if remaining > 0 {
                self.fail_network.fetch_sub(1, Ordering::SeqCst);
                return Err(ApplyError::Network);
            }
            Ok(())
        }
    }

    impl MailSession for FakeProvider {
        fn sync_one_folder(
            &mut self,
            _store: &mut CoreSyncStore<'_>,
            _folder: &str,
            _now: i64,
            _is_cancelled: &dyn Fn() -> bool,
        ) -> Result<SyncOutcome, SyncError> {
            Err(SyncError::Session(
                "FakeProvider never syncs a folder for real: none of the queue-focused \
                 tests below ever reach a folder with a resolvable remote_id"
                    .into(),
            ))
        }

        fn open_one_body(
            &mut self,
            _core: &mut Core,
            _id: &MessageId,
            _bodies_dir: &Path,
        ) -> Result<Vec<u8>, CoreError> {
            Err(CoreError::new(
                ErrorCode::NetworkUnavailable,
                "FakeProvider never fetches a body for real: none of the tests exercising it \
                 send FetchBody",
            ))
        }

        fn idle_once(
            &mut self,
            _folder: &str,
            _idle_timeout: Duration,
            _should_stop: &mut dyn FnMut() -> bool,
        ) -> Result<IdleRound, ConnectError> {
            Err(ConnectError::network(
                "FakeProvider never idles for real: none of the tests exercising it seed a \
                 resolvable Inbox folder, so watch_inbox_for_push always short-circuits to \
                 NoInboxFolder before reaching idle_once",
            ))
        }
    }

    /// Hands out [`FakeProvider`]s sharing the same counters across
    /// however many times the worker reconnects, and counts `connect`
    /// calls itself (used to prove the worker isn't reconnecting on every
    /// single retry -- see `retry_backs_off_and_does_not_hot_loop`).
    struct FakeFactory {
        fail_network: Arc<AtomicU32>,
        applies: Arc<AtomicU32>,
        connects: Arc<AtomicU32>,
    }

    impl FakeFactory {
        fn new(fail_network: u32) -> (Self, Arc<AtomicU32>, Arc<AtomicU32>) {
            let applies = Arc::new(AtomicU32::new(0));
            let connects = Arc::new(AtomicU32::new(0));
            (
                Self {
                    fail_network: Arc::new(AtomicU32::new(fail_network)),
                    applies: Arc::clone(&applies),
                    connects: Arc::clone(&connects),
                },
                applies,
                connects,
            )
        }
    }

    impl ProviderFactory for FakeFactory {
        fn connect(&mut self, _account: &AccountId) -> Result<Box<dyn MailSession>, ConnectError> {
            self.connects.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(FakeProvider {
                fail_network: Arc::clone(&self.fail_network),
                applies: Arc::clone(&self.applies),
            }))
        }
    }

    /// A [`MailSession`] double dedicated to T-078 (b)'s own tests: unlike
    /// [`FakeProvider`]/[`PerAccountProvider`] above (whose
    /// [`MailSession::sync_one_folder`] always errors, because none of the
    /// pre-existing queue-focused tests ever reach it), this one lets a
    /// test control success/failure and records every remote mailbox name
    /// [`sync_one_due_folder`] resolved and passed in -- proof the
    /// wiring reached [`feathermail_core::Core::remote_folder`] and handed
    /// its *result* onward, not the local `folders.id`.
    struct SyncSessionProvider {
        fail: bool,
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl MailProvider for SyncSessionProvider {
        fn apply(&mut self, _op: &Operation) -> Result<(), ApplyError> {
            Ok(())
        }
    }

    impl MailSession for SyncSessionProvider {
        fn sync_one_folder(
            &mut self,
            store: &mut CoreSyncStore<'_>,
            folder: &str,
            now: i64,
            is_cancelled: &dyn Fn() -> bool,
        ) -> Result<SyncOutcome, SyncError> {
            self.calls.lock().unwrap().push(folder.to_string());
            // Exercised for real (not just present in the signature) so a
            // test asserting on cancellation isn't testing a path this
            // fake secretly skips -- see
            // `sync_one_due_folder_consumes_a_shutdown_sent_during_the_sync_pass_and_reports_it`.
            let cancelled = is_cancelled();
            if self.fail {
                return Err(SyncError::Session("simulated sync failure".into()));
            }
            // A successful pass leaves `last_synced_at = now` behind,
            // exactly as `feathermail_sync::sync_folder` does through
            // `SyncStore::save_state`. That is the field `next_sync` reads
            // for its normal due-at threshold, so a double that skips the
            // stamp reports the same folder due on every single pass
            // forever -- which is only invisible while the test's clock
            // never moves (T-114).
            let mut state = store.load_state(folder)?;
            state.last_synced_at = Some(now);
            store.save_state(folder, &state)?;
            Ok(SyncOutcome {
                folder: folder.to_string(),
                cancelled,
                ..Default::default()
            })
        }

        fn open_one_body(
            &mut self,
            _core: &mut Core,
            _id: &MessageId,
            _bodies_dir: &Path,
        ) -> Result<Vec<u8>, CoreError> {
            // T-119: a successful header pass now queues a warm-up for
            // whatever in this folder still has no body. Tests that do
            // not seed such a row never reach this; the one that does
            // needs an honest success so BodyReady is `ok: true` without
            // the test itself sending FetchBody.
            Ok(b"simulated raw RFC 822 body".to_vec())
        }

        /// Exercised for real (T-089), unlike `open_one_body` above: this
        /// is the only fake in this module whose single-account tests can
        /// ever reach `watch_inbox_for_push` with a resolvable Inbox (see
        /// `seed_account_with_due_folder`'s `kind = 'inbox'`), so it is
        /// the one that has to prove what `idle_timeout` it was actually
        /// handed rather than just erroring like the others below.
        /// Records into the same `calls` log `sync_one_folder` uses, with
        /// an `IDLE:` prefix so a test can tell the two kinds of call
        /// apart, and returns immediately (`TimedOut`) so a test never
        /// waits out a real ceiling -- see
        /// `a_folder_due_soon_shortens_the_idle_sleep_instead_of_waiting_out_the_full_poll`.
        fn idle_once(
            &mut self,
            folder: &str,
            idle_timeout: Duration,
            _should_stop: &mut dyn FnMut() -> bool,
        ) -> Result<IdleRound, ConnectError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("IDLE:{folder}:{}", idle_timeout.as_secs()));
            Ok(IdleRound {
                idle_capable: true,
                outcome: IdleOutcome::TimedOut,
            })
        }
    }

    /// T-118: a header pass that actually polls `is_cancelled` between
    /// batches, the way [`feathermail_sync::sync_folder`] does. The
    /// one-shot fake above records the drain but always finishes; this
    /// one sits in the loop until told to yield, so a test can prove a
    /// click aborts the pass instead of waiting it out.
    struct YieldingSyncSession {
        give_up: Duration,
        cancelled: Arc<AtomicBool>,
    }

    impl MailProvider for YieldingSyncSession {
        fn apply(&mut self, _op: &Operation) -> Result<(), ApplyError> {
            Ok(())
        }
    }

    impl MailSession for YieldingSyncSession {
        fn sync_one_folder(
            &mut self,
            _store: &mut CoreSyncStore<'_>,
            folder: &str,
            _now: i64,
            is_cancelled: &dyn Fn() -> bool,
        ) -> Result<SyncOutcome, SyncError> {
            let started = Instant::now();
            loop {
                if is_cancelled() {
                    self.cancelled.store(true, Ordering::SeqCst);
                    return Ok(SyncOutcome {
                        folder: folder.to_string(),
                        cancelled: true,
                        ..Default::default()
                    });
                }
                if started.elapsed() >= self.give_up {
                    return Ok(SyncOutcome {
                        folder: folder.to_string(),
                        cancelled: false,
                        ..Default::default()
                    });
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        fn open_one_body(
            &mut self,
            _core: &mut Core,
            _id: &MessageId,
            _bodies_dir: &Path,
        ) -> Result<Vec<u8>, CoreError> {
            Err(CoreError::new(
                ErrorCode::NetworkUnavailable,
                "YieldingSyncSession is a header-pass fake",
            ))
        }

        fn idle_once(
            &mut self,
            _folder: &str,
            _idle_timeout: Duration,
            _should_stop: &mut dyn FnMut() -> bool,
        ) -> Result<IdleRound, ConnectError> {
            Ok(IdleRound {
                idle_capable: true,
                outcome: IdleOutcome::TimedOut,
            })
        }
    }

    /// Small session double for the new-account bootstrap: it exposes a
    /// server `LIST` result but intentionally does no network work in the
    /// unrelated queue/body/IDLE doors.
    struct FolderDiscoverySession {
        calls: Arc<AtomicU32>,
    }

    impl MailProvider for FolderDiscoverySession {
        fn apply(&mut self, _op: &Operation) -> Result<(), ApplyError> {
            Ok(())
        }
    }

    impl MailSession for FolderDiscoverySession {
        fn discover_folders(&mut self) -> Result<Vec<DiscoveredFolder>, ConnectError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![DiscoveredFolder {
                remote_id: "INBOX".into(),
                kind: FolderKind::Inbox,
                label: "Inbox".into(),
                parent_remote_id: None,
                delimiter: Some('/'),
            }])
        }

        fn sync_one_folder(
            &mut self,
            _store: &mut CoreSyncStore<'_>,
            _folder: &str,
            _now: i64,
            _is_cancelled: &dyn Fn() -> bool,
        ) -> Result<SyncOutcome, SyncError> {
            unreachable!("bootstrap test only discovers folders")
        }

        fn open_one_body(
            &mut self,
            _core: &mut Core,
            _id: &MessageId,
            _bodies_dir: &Path,
        ) -> Result<Vec<u8>, CoreError> {
            unreachable!("bootstrap test only discovers folders")
        }

        fn idle_once(
            &mut self,
            _folder: &str,
            _idle_timeout: Duration,
            _should_stop: &mut dyn FnMut() -> bool,
        ) -> Result<IdleRound, ConnectError> {
            unreachable!("bootstrap test only discovers folders")
        }
    }

    /// Hands out [`SyncSessionProvider`]s that all share one `fail` flag
    /// and one `calls` log, so a test can flip whether the *next* connect
    /// produces a succeeding or failing session and still see every
    /// folder name synced across however many reconnects happen.
    struct SyncSessionFactory {
        fail: Arc<std::sync::atomic::AtomicBool>,
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl ProviderFactory for SyncSessionFactory {
        fn connect(&mut self, _account: &AccountId) -> Result<Box<dyn MailSession>, ConnectError> {
            Ok(Box::new(SyncSessionProvider {
                fail: self.fail.load(Ordering::SeqCst),
                calls: Arc::clone(&self.calls),
            }))
        }
    }

    /// A [`MailSession`] double dedicated to T-091's own tests: unlike
    /// [`SyncSessionProvider`] above (whose `fail` flag only ever
    /// produces `SyncError::Session`, T-078 (b)'s stand-in for "some
    /// generic sync failure"), this one lets a test pick exactly which
    /// [`feathermail_sync::SyncError`] variant -- or success -- the next
    /// `sync_one_folder` call returns, which is what a direct-call test
    /// needs to exercise `SyncError::Auth` specifically without dragging
    /// in a whole `ProviderFactory`.
    struct AuthAwareSession {
        fail: Option<SyncError>,
    }

    impl MailProvider for AuthAwareSession {
        fn apply(&mut self, _op: &Operation) -> Result<(), ApplyError> {
            Ok(())
        }
    }

    impl MailSession for AuthAwareSession {
        fn sync_one_folder(
            &mut self,
            _store: &mut CoreSyncStore<'_>,
            folder: &str,
            _now: i64,
            _is_cancelled: &dyn Fn() -> bool,
        ) -> Result<SyncOutcome, SyncError> {
            match &self.fail {
                None => Ok(SyncOutcome {
                    folder: folder.to_string(),
                    ..Default::default()
                }),
                Some(err) => Err(err.clone()),
            }
        }

        fn open_one_body(
            &mut self,
            _core: &mut Core,
            _id: &MessageId,
            _bodies_dir: &Path,
        ) -> Result<Vec<u8>, CoreError> {
            Err(CoreError::new(
                ErrorCode::NetworkUnavailable,
                "AuthAwareSession never fetches a body for real: none of the tests exercising \
                 it send FetchBody",
            ))
        }

        fn idle_once(
            &mut self,
            _folder: &str,
            _idle_timeout: Duration,
            _should_stop: &mut dyn FnMut() -> bool,
        ) -> Result<IdleRound, ConnectError> {
            Err(ConnectError::network(
                "AuthAwareSession never idles for real: none of the tests exercising it seed a \
                 resolvable Inbox folder",
            ))
        }
    }

    /// A [`MailSession`]/[`ProviderFactory`] pair dedicated to T-091's own
    /// worker-level (through [`start_with_clock`]) tests: each session
    /// [`ProviderFactory::connect`] hands out carries a distinct `id` (an
    /// incrementing counter owned by the factory), and every
    /// `sync_one_folder` call records which `id` handled it into
    /// `session_ids_used`. `auth_failures_left` counts down: while it is
    /// above zero, every `sync_one_folder` call fails with
    /// [`feathermail_sync::SyncError::Auth`]; once it hits zero, every
    /// call after that succeeds. This is what lets a test tell "the
    /// worker kept retrying against the same dead session forever" (the
    /// bug T-091 exists to fix) apart from "the worker dropped it and
    /// reconnected" -- by session *identity*, not merely by whether mail
    /// eventually got synced.
    struct AuthFailProvider {
        id: u32,
        auth_failures_left: Arc<AtomicU32>,
        session_ids_used: Arc<Mutex<Vec<u32>>>,
    }

    impl MailProvider for AuthFailProvider {
        fn apply(&mut self, _op: &Operation) -> Result<(), ApplyError> {
            Ok(())
        }
    }

    impl MailSession for AuthFailProvider {
        fn sync_one_folder(
            &mut self,
            _store: &mut CoreSyncStore<'_>,
            folder: &str,
            _now: i64,
            _is_cancelled: &dyn Fn() -> bool,
        ) -> Result<SyncOutcome, SyncError> {
            self.session_ids_used.lock().unwrap().push(self.id);
            let remaining = self.auth_failures_left.load(Ordering::SeqCst);
            if remaining > 0 {
                self.auth_failures_left.fetch_sub(1, Ordering::SeqCst);
                return Err(SyncError::Auth);
            }
            Ok(SyncOutcome {
                folder: folder.to_string(),
                ..Default::default()
            })
        }

        fn open_one_body(
            &mut self,
            _core: &mut Core,
            _id: &MessageId,
            _bodies_dir: &Path,
        ) -> Result<Vec<u8>, CoreError> {
            Err(CoreError::new(
                ErrorCode::NetworkUnavailable,
                "AuthFailProvider never fetches a body for real: none of the tests exercising \
                 it send FetchBody",
            ))
        }

        fn idle_once(
            &mut self,
            _folder: &str,
            _idle_timeout: Duration,
            _should_stop: &mut dyn FnMut() -> bool,
        ) -> Result<IdleRound, ConnectError> {
            Err(ConnectError::network(
                "AuthFailProvider never idles for real: none of the tests exercising it seed a \
                 resolvable Inbox folder, so watch_inbox_for_push always short-circuits to \
                 NoInboxFolder before reaching idle_once",
            ))
        }
    }

    struct AuthFailFactory {
        next_id: Arc<AtomicU32>,
        auth_failures_left: Arc<AtomicU32>,
        session_ids_used: Arc<Mutex<Vec<u32>>>,
        connects: Arc<AtomicU32>,
    }

    impl ProviderFactory for AuthFailFactory {
        fn connect(&mut self, _account: &AccountId) -> Result<Box<dyn MailSession>, ConnectError> {
            self.connects.fetch_add(1, Ordering::SeqCst);
            let id = self.next_id.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(AuthFailProvider {
                id,
                auth_failures_left: Arc::clone(&self.auth_failures_left),
                session_ids_used: Arc::clone(&self.session_ids_used),
            }))
        }
    }

    /// A [`MailSession`] for T-091 (б)'s connection-vs-store distinction
    /// tests: each `sync_one_folder` call pops the next scripted outcome
    /// off a shared queue (`Ok` once the queue is drained), and records
    /// which connected session `id` handled it into `session_ids_used` --
    /// the same session-identity trick [`AuthFailProvider`] uses, just
    /// driven by an arbitrary ordered sequence of distinct
    /// [`feathermail_sync::SyncError`] variants instead of one repeated
    /// kind. That is what lets a single test drive, say, a `Store`
    /// failure (must not drop the session) immediately followed by a
    /// `Session` failure (must drop it) against the very same running
    /// worker, and read the difference straight off session identity.
    struct ScriptedFailProvider {
        id: u32,
        script: Arc<Mutex<VecDeque<SyncError>>>,
        session_ids_used: Arc<Mutex<Vec<u32>>>,
    }

    impl MailProvider for ScriptedFailProvider {
        fn apply(&mut self, _op: &Operation) -> Result<(), ApplyError> {
            Ok(())
        }
    }

    impl MailSession for ScriptedFailProvider {
        fn sync_one_folder(
            &mut self,
            _store: &mut CoreSyncStore<'_>,
            folder: &str,
            _now: i64,
            _is_cancelled: &dyn Fn() -> bool,
        ) -> Result<SyncOutcome, SyncError> {
            self.session_ids_used.lock().unwrap().push(self.id);
            match self.script.lock().unwrap().pop_front() {
                Some(err) => Err(err),
                None => Ok(SyncOutcome {
                    folder: folder.to_string(),
                    ..Default::default()
                }),
            }
        }

        fn open_one_body(
            &mut self,
            _core: &mut Core,
            _id: &MessageId,
            _bodies_dir: &Path,
        ) -> Result<Vec<u8>, CoreError> {
            Err(CoreError::new(
                ErrorCode::NetworkUnavailable,
                "ScriptedFailProvider never fetches a body for real: none of the tests \
                 exercising it send FetchBody",
            ))
        }

        fn idle_once(
            &mut self,
            _folder: &str,
            _idle_timeout: Duration,
            _should_stop: &mut dyn FnMut() -> bool,
        ) -> Result<IdleRound, ConnectError> {
            Err(ConnectError::network(
                "ScriptedFailProvider never idles for real: none of the tests exercising it \
                 seed a resolvable Inbox folder, so watch_inbox_for_push always short-circuits \
                 to NoInboxFolder before reaching idle_once",
            ))
        }
    }

    struct ScriptedFailFactory {
        next_id: Arc<AtomicU32>,
        script: Arc<Mutex<VecDeque<SyncError>>>,
        session_ids_used: Arc<Mutex<Vec<u32>>>,
        connects: Arc<AtomicU32>,
    }

    impl ProviderFactory for ScriptedFailFactory {
        fn connect(&mut self, _account: &AccountId) -> Result<Box<dyn MailSession>, ConnectError> {
            self.connects.fetch_add(1, Ordering::SeqCst);
            let id = self.next_id.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(ScriptedFailProvider {
                id,
                script: Arc::clone(&self.script),
                session_ids_used: Arc::clone(&self.session_ids_used),
            }))
        }
    }

    /// A [`ProviderFactory`] whose providers remember which account they
    /// were opened for, and record `(account, target_id)` for every apply.
    /// This is what lets a test see the difference between "the operation
    /// was applied" and "the operation was applied *on the right
    /// account's connection*" -- the whole point of
    /// [`Core::tick_for_account`].
    #[derive(Clone, Default)]
    struct PerAccountFactory {
        applies: Arc<Mutex<Vec<(String, String)>>>,
    }

    struct PerAccountProvider {
        account: String,
        applies: Arc<Mutex<Vec<(String, String)>>>,
    }

    impl MailProvider for PerAccountProvider {
        fn apply(&mut self, op: &Operation) -> Result<(), ApplyError> {
            self.applies
                .lock()
                .unwrap()
                .push((self.account.clone(), op.target_id.clone()));
            Ok(())
        }
    }

    impl MailSession for PerAccountProvider {
        fn sync_one_folder(
            &mut self,
            _store: &mut CoreSyncStore<'_>,
            _folder: &str,
            _now: i64,
            _is_cancelled: &dyn Fn() -> bool,
        ) -> Result<SyncOutcome, SyncError> {
            Err(SyncError::Session(
                "PerAccountProvider never syncs a folder for real: none of the queue-focused \
                 tests below ever reach a folder with a resolvable remote_id"
                    .into(),
            ))
        }

        fn open_one_body(
            &mut self,
            _core: &mut Core,
            _id: &MessageId,
            _bodies_dir: &Path,
        ) -> Result<Vec<u8>, CoreError> {
            Err(CoreError::new(
                ErrorCode::NetworkUnavailable,
                "PerAccountProvider never fetches a body for real: none of the tests exercising it \
                 send FetchBody",
            ))
        }

        fn idle_once(
            &mut self,
            _folder: &str,
            _idle_timeout: Duration,
            _should_stop: &mut dyn FnMut() -> bool,
        ) -> Result<IdleRound, ConnectError> {
            Err(ConnectError::network(
                "PerAccountProvider never idles for real: none of the tests exercising it seed \
                 a resolvable Inbox folder, so watch_inbox_for_push always short-circuits to \
                 NoInboxFolder before reaching idle_once",
            ))
        }
    }

    impl ProviderFactory for PerAccountFactory {
        fn connect(&mut self, account: &AccountId) -> Result<Box<dyn MailSession>, ConnectError> {
            Ok(Box::new(PerAccountProvider {
                account: account.0.clone(),
                applies: Arc::clone(&self.applies),
            }))
        }
    }

    /// A [`ProviderFactory`] that refuses to connect for some accounts and
    /// succeeds for the rest, counting attempts per account.
    #[derive(Clone)]
    struct SelectiveFactory {
        unreachable: Vec<String>,
        attempts: Arc<Mutex<Vec<String>>>,
        applies: Arc<Mutex<Vec<(String, String)>>>,
    }

    impl ProviderFactory for SelectiveFactory {
        fn connect(&mut self, account: &AccountId) -> Result<Box<dyn MailSession>, ConnectError> {
            self.attempts.lock().unwrap().push(account.0.clone());
            if self.unreachable.iter().any(|id| id == &account.0) {
                return Err(ConnectError::network("host is not answering"));
            }
            Ok(Box::new(PerAccountProvider {
                account: account.0.clone(),
                applies: Arc::clone(&self.applies),
            }))
        }
    }

    /// A [`MailSession`] double dedicated to T-080's own tests:
    /// [`MailSession::open_one_body`] is exercised for real (recording
    /// which account/message it was called for) instead of erroring like
    /// [`FakeProvider`]/[`PerAccountProvider`]/[`SyncSessionProvider`]
    /// above -- none of those pre-existing fakes ever need to fetch a
    /// body, so making them do so would be adding an untested path to a
    /// fake, not proving anything about `run`.
    struct BodyFetchProvider {
        account: String,
        fail_body: bool,
        body_calls: Arc<Mutex<Vec<(String, MessageId)>>>,
        attachment_calls: Arc<Mutex<Vec<(String, AttachmentId)>>>,
    }

    impl MailProvider for BodyFetchProvider {
        fn apply(&mut self, _op: &Operation) -> Result<(), ApplyError> {
            Ok(())
        }
    }

    impl MailSession for BodyFetchProvider {
        fn sync_one_folder(
            &mut self,
            _store: &mut CoreSyncStore<'_>,
            _folder: &str,
            _now: i64,
            _is_cancelled: &dyn Fn() -> bool,
        ) -> Result<SyncOutcome, SyncError> {
            Err(SyncError::Session(
                "BodyFetchProvider never syncs a folder for real: none of T-080's own \
                 tests seed a due folder"
                    .into(),
            ))
        }

        fn open_one_body(
            &mut self,
            _core: &mut Core,
            id: &MessageId,
            _bodies_dir: &Path,
        ) -> Result<Vec<u8>, CoreError> {
            self.body_calls
                .lock()
                .unwrap()
                .push((self.account.clone(), id.clone()));
            if self.fail_body {
                return Err(CoreError::new(
                    ErrorCode::NetworkUnavailable,
                    "simulated body fetch failure",
                ));
            }
            Ok(b"simulated raw RFC 822 body".to_vec())
        }

        fn download_one_attachment(
            &mut self,
            _core: &mut Core,
            _account_id: &AccountId,
            attachment_id: &AttachmentId,
            _attachments_dir: &Path,
        ) -> Result<(), CoreError> {
            self.attachment_calls
                .lock()
                .unwrap()
                .push((self.account.clone(), attachment_id.clone()));
            Ok(())
        }

        fn idle_once(
            &mut self,
            _folder: &str,
            _idle_timeout: Duration,
            _should_stop: &mut dyn FnMut() -> bool,
        ) -> Result<IdleRound, ConnectError> {
            Err(ConnectError::network(
                "BodyFetchProvider never idles for real: none of T-080's own tests seed a \
                 resolvable Inbox folder, so watch_inbox_for_push always short-circuits to \
                 NoInboxFolder before reaching idle_once",
            ))
        }
    }

    /// Hands out [`BodyFetchProvider`]s, optionally refusing to connect at
    /// all for some accounts (mirrors [`SelectiveFactory`] above, but
    /// wired to a session that can actually serve `open_one_body`).
    #[derive(Clone)]
    struct BodyFetchFactory {
        unreachable: Vec<String>,
        fail_body: bool,
        connects: Arc<Mutex<Vec<String>>>,
        body_calls: Arc<Mutex<Vec<(String, MessageId)>>>,
        attachment_calls: Arc<Mutex<Vec<(String, AttachmentId)>>>,
    }

    impl ProviderFactory for BodyFetchFactory {
        fn connect(&mut self, account: &AccountId) -> Result<Box<dyn MailSession>, ConnectError> {
            self.connects.lock().unwrap().push(account.0.clone());
            if self.unreachable.iter().any(|id| id == &account.0) {
                return Err(ConnectError::network("host is not answering"));
            }
            Ok(Box::new(BodyFetchProvider {
                account: account.0.clone(),
                fail_body: self.fail_body,
                body_calls: Arc::clone(&self.body_calls),
                attachment_calls: Arc::clone(&self.attachment_calls),
            }))
        }
    }

    /// A [`ProviderFactory`] that parks inside `connect` -- standing in
    /// for a real `ImapSession::connect` sitting on a socket -- until the
    /// test lets it go. Used to prove dropping a [`SyncHandle`] does not
    /// wait for whatever the worker is currently inside.
    struct BlockingFactory {
        entered: Arc<AtomicU32>,
        release: Arc<(Mutex<bool>, Condvar)>,
        left: Arc<AtomicU32>,
    }

    impl ProviderFactory for BlockingFactory {
        fn connect(&mut self, _account: &AccountId) -> Result<Box<dyn MailSession>, ConnectError> {
            self.entered.fetch_add(1, Ordering::SeqCst);
            let (lock, cv) = &*self.release;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = cv.wait(released).unwrap();
            }
            self.left.fetch_add(1, Ordering::SeqCst);
            Err(ConnectError::network("released"))
        }
    }

    /// A [`MailSession`] double dedicated to T-089's own tests: unlike
    /// [`SyncSessionProvider`]'s `idle_once` (which records the call and
    /// returns immediately, T-078 (b) style), this one's `idle_once`
    /// genuinely blocks, re-checking `should_stop` on a real (but tiny,
    /// 2ms) interval up to the requested ceiling -- real timing,
    /// deliberately, so a test proving a command sent *during* IDLE is
    /// noticed in well under the ceiling is proving something about the
    /// actual `should_stop` plumbing, not about a fake that happens to
    /// return quickly regardless. `fail_idle_times`, when nonzero,
    /// short-circuits `idle_once` straight to `Err` instead -- standing in
    /// for a connection that drops mid-IDLE (RFC 2177).
    struct IdleBlockingProvider {
        account: String,
        idle_calls: Arc<AtomicU32>,
        idle_folders: Arc<Mutex<Vec<String>>>,
        body_calls: Arc<Mutex<Vec<(String, MessageId)>>>,
        fail_idle_times: Arc<AtomicU32>,
    }

    impl MailProvider for IdleBlockingProvider {
        fn apply(&mut self, _op: &Operation) -> Result<(), ApplyError> {
            Ok(())
        }
    }

    impl MailSession for IdleBlockingProvider {
        fn sync_one_folder(
            &mut self,
            _store: &mut CoreSyncStore<'_>,
            _folder: &str,
            _now: i64,
            _is_cancelled: &dyn Fn() -> bool,
        ) -> Result<SyncOutcome, SyncError> {
            Err(SyncError::Session(
                "IdleBlockingProvider never syncs a folder for real: none of T-089's own \
                 tests drive idle_once to report Events"
                    .into(),
            ))
        }

        fn open_one_body(
            &mut self,
            _core: &mut Core,
            id: &MessageId,
            _bodies_dir: &Path,
        ) -> Result<Vec<u8>, CoreError> {
            self.body_calls
                .lock()
                .unwrap()
                .push((self.account.clone(), id.clone()));
            Ok(b"simulated raw RFC 822 body".to_vec())
        }

        fn idle_once(
            &mut self,
            folder: &str,
            idle_timeout: Duration,
            should_stop: &mut dyn FnMut() -> bool,
        ) -> Result<IdleRound, ConnectError> {
            self.idle_calls.fetch_add(1, Ordering::SeqCst);
            self.idle_folders.lock().unwrap().push(folder.to_string());
            // Decrement-and-check atomically (fetch_update) rather than
            // load-then-store: two tests below run this fake single
            // threaded, but a racy load/store here would be exactly the
            // kind of bug that only shows up once, under load, in someone
            // else's test later.
            let should_fail = self
                .fail_idle_times
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                    if n > 0 {
                        Some(n - 1)
                    } else {
                        None
                    }
                })
                .is_ok();
            if should_fail {
                return Err(ConnectError::network("simulated connection drop mid-IDLE"));
            }
            let deadline = Instant::now() + idle_timeout;
            loop {
                if should_stop() {
                    return Ok(IdleRound {
                        idle_capable: true,
                        outcome: IdleOutcome::Stopped,
                    });
                }
                if Instant::now() >= deadline {
                    return Ok(IdleRound {
                        idle_capable: true,
                        outcome: IdleOutcome::TimedOut,
                    });
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }

    /// Hands out [`IdleBlockingProvider`]s that all share one
    /// `idle_calls`/`body_calls`/`fail_idle_times` set, and counts
    /// `connect` calls itself -- what lets a test prove a connection lost
    /// mid-`IDLE` reconnects through this exact same `connect`, the same
    /// way an ordinary apply/connect failure already does (T-089, fork
    /// #4: no second, parallel reconnect mechanism).
    #[derive(Clone)]
    struct IdleBlockingFactory {
        connects: Arc<AtomicU32>,
        idle_calls: Arc<AtomicU32>,
        idle_folders: Arc<Mutex<Vec<String>>>,
        body_calls: Arc<Mutex<Vec<(String, MessageId)>>>,
        fail_idle_times: Arc<AtomicU32>,
    }

    impl ProviderFactory for IdleBlockingFactory {
        fn connect(&mut self, account: &AccountId) -> Result<Box<dyn MailSession>, ConnectError> {
            self.connects.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(IdleBlockingProvider {
                account: account.0.clone(),
                idle_calls: Arc::clone(&self.idle_calls),
                idle_folders: Arc::clone(&self.idle_folders),
                body_calls: Arc::clone(&self.body_calls),
                fail_idle_times: Arc::clone(&self.fail_idle_times),
            }))
        }
    }

    /// T-118: a first-time header backfill that occupies the live
    /// session for tens of seconds unless `is_cancelled` yields. The
    /// owner's All-accounts click sat behind this: FetchBody was queued
    /// and the pass ran to the last UID.
    struct LongBackfillProvider {
        account: String,
        sync_started: Arc<AtomicU32>,
        body_calls: Arc<Mutex<Vec<(String, MessageId)>>>,
    }

    impl MailProvider for LongBackfillProvider {
        fn apply(&mut self, _op: &Operation) -> Result<(), ApplyError> {
            Ok(())
        }
    }

    impl MailSession for LongBackfillProvider {
        fn sync_one_folder(
            &mut self,
            store: &mut CoreSyncStore<'_>,
            folder: &str,
            now: i64,
            is_cancelled: &dyn Fn() -> bool,
        ) -> Result<SyncOutcome, SyncError> {
            self.sync_started.fetch_add(1, Ordering::SeqCst);
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                if is_cancelled() {
                    let mut state = store.load_state(folder)?;
                    state.last_synced_at = Some(now);
                    store.save_state(folder, &state)?;
                    return Ok(SyncOutcome {
                        folder: folder.to_string(),
                        cancelled: true,
                        ..Default::default()
                    });
                }
                if Instant::now() >= deadline {
                    let mut state = store.load_state(folder)?;
                    state.last_synced_at = Some(now);
                    store.save_state(folder, &state)?;
                    return Ok(SyncOutcome {
                        folder: folder.to_string(),
                        cancelled: false,
                        ..Default::default()
                    });
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        fn open_one_body(
            &mut self,
            _core: &mut Core,
            id: &MessageId,
            _bodies_dir: &Path,
        ) -> Result<Vec<u8>, CoreError> {
            self.body_calls
                .lock()
                .unwrap()
                .push((self.account.clone(), id.clone()));
            Ok(b"simulated raw RFC 822 body".to_vec())
        }

        fn idle_once(
            &mut self,
            _folder: &str,
            _idle_timeout: Duration,
            _should_stop: &mut dyn FnMut() -> bool,
        ) -> Result<IdleRound, ConnectError> {
            Ok(IdleRound {
                idle_capable: true,
                outcome: IdleOutcome::TimedOut,
            })
        }
    }

    struct LongBackfillFactory {
        sync_started: Arc<AtomicU32>,
        body_calls: Arc<Mutex<Vec<(String, MessageId)>>>,
    }

    impl ProviderFactory for LongBackfillFactory {
        fn connect(&mut self, account: &AccountId) -> Result<Box<dyn MailSession>, ConnectError> {
            Ok(Box::new(LongBackfillProvider {
                account: account.0.clone(),
                sync_started: Arc::clone(&self.sync_started),
                body_calls: Arc::clone(&self.body_calls),
            }))
        }
    }

    /// A [`MailSession`] double for a server that never supports `IMAP
    /// IDLE` at all (D30's `!caps.idle` case): every `idle_once` call
    /// returns immediately -- `idle_capable: false`,
    /// `IdleOutcome::TimedOut` -- exactly what
    /// `feathermail_providers::run_idle_with`'s own `!caps.idle` branch
    /// does against a real server (one `NOOP`, no wait of its own).
    /// Counts calls so a test can bound how many round trips the worker
    /// makes against this "server" over a real-time observation window --
    /// the whole point being that this fake, unlike
    /// [`IdleBlockingProvider`], can never itself introduce a delay, so
    /// any pacing observed has to come from `watch_inbox_for_push` itself
    /// honoring `NO_IDLE_POLL_SECS`, not from this fake stalling.
    struct NoIdleHammerProvider {
        idle_calls: Arc<AtomicU32>,
    }

    impl MailProvider for NoIdleHammerProvider {
        fn apply(&mut self, _op: &Operation) -> Result<(), ApplyError> {
            Ok(())
        }
    }

    impl MailSession for NoIdleHammerProvider {
        fn sync_one_folder(
            &mut self,
            _store: &mut CoreSyncStore<'_>,
            _folder: &str,
            _now: i64,
            _is_cancelled: &dyn Fn() -> bool,
        ) -> Result<SyncOutcome, SyncError> {
            Err(SyncError::Session(
                "NoIdleHammerProvider never syncs a folder for real: idle_once always \
                 reports TimedOut, never Events"
                    .into(),
            ))
        }

        fn open_one_body(
            &mut self,
            _core: &mut Core,
            _id: &MessageId,
            _bodies_dir: &Path,
        ) -> Result<Vec<u8>, CoreError> {
            Err(CoreError::new(
                ErrorCode::NetworkUnavailable,
                "NoIdleHammerProvider never fetches a body for real",
            ))
        }

        fn idle_once(
            &mut self,
            _folder: &str,
            _idle_timeout: Duration,
            _should_stop: &mut dyn FnMut() -> bool,
        ) -> Result<IdleRound, ConnectError> {
            self.idle_calls.fetch_add(1, Ordering::SeqCst);
            Ok(IdleRound {
                idle_capable: false,
                outcome: IdleOutcome::TimedOut,
            })
        }
    }

    #[derive(Clone)]
    struct NoIdleHammerFactory {
        idle_calls: Arc<AtomicU32>,
    }

    impl ProviderFactory for NoIdleHammerFactory {
        fn connect(&mut self, _account: &AccountId) -> Result<Box<dyn MailSession>, ConnectError> {
            Ok(Box::new(NoIdleHammerProvider {
                idle_calls: Arc::clone(&self.idle_calls),
            }))
        }
    }

    /// [`WorkerClock`] double: advances its own logical `now` by exactly
    /// the requested timeout instead of actually sleeping, so a test can
    /// drive multi-second D32 backoff delays without waiting them out in
    /// real time. Still gives a real, tiny (20ms) window for an
    /// already-sent [`WorkerCommand`] to be observed -- that is ordinary
    /// channel-synchronization slack for the test's own `shutdown()` /
    /// implicit `Drop`, not the backoff delay under test.
    struct FakeClock {
        now: Arc<Mutex<i64>>,
    }

    impl FakeClock {
        fn new(start: i64) -> Self {
            Self {
                now: Arc::new(Mutex::new(start)),
            }
        }
    }

    /// T-090: a [`PowerProbe`] that always answers the same thing, so a
    /// test decides whether the simulated machine is on battery without
    /// touching real sysfs (which the CI host may not even have).
    /// `FixedPower(false)` is also what every pre-T-090 test below gets:
    /// their scenarios never involve power saving, and the fixed `false`
    /// keeps their schedules byte-identical to what they were written
    /// against.
    struct FixedPower(bool);

    impl PowerProbe for FixedPower {
        fn on_battery(&self) -> bool {
            self.0
        }
    }

    /// T-090: builds the neutral [`ScheduleSnapshot`] a direct
    /// `sync_one_due_folder` call needs when the test does not care about
    /// focus/power -- i.e. exactly the pre-T-090 defaults. Tests that *do*
    /// care build the struct literally instead, so the value under test is
    /// visible at the call site.
    fn schedule_at(now: i64) -> ScheduleSnapshot {
        ScheduleSnapshot {
            now,
            view: Viewport::default(),
            power: PowerState::default(),
        }
    }

    impl WorkerClock for FakeClock {
        fn now(&self) -> i64 {
            *self.now.lock().unwrap()
        }

        fn wait(&self, rx: &Receiver<WorkerCommand>, timeout_secs: i64) -> Option<WorkerCommand> {
            if let Ok(cmd) = rx.recv_timeout(Duration::from_millis(20)) {
                return Some(cmd);
            }
            *self.now.lock().unwrap() += timeout_secs.max(0);
            None
        }
    }

    impl FakeClock {
        /// A second handle onto this clock's internal `now`, taken before
        /// handing `self` by value to [`start_with_clock`] (which consumes
        /// it). Lets a test read back exactly how many simulated seconds
        /// the worker actually waited -- see
        /// `combined_idle_wait_after_a_folder_hint_and_a_queue_retry_takes_the_shorter_one_not_the_sum`,
        /// which cannot distinguish "waited the minimum" from "waited the
        /// sum" any other way: [`FakeClock::wait`] never really sleeps
        /// for more than 20ms of *real* time no matter what `timeout_secs`
        /// it's asked for, so wall-clock elapsed time is useless here --
        /// only the simulated clock's own advancement tells the two apart.
        fn shared_now(&self) -> Arc<Mutex<i64>> {
            Arc::clone(&self.now)
        }
    }

    // --- profile seeding: a raw connection to the same on-disk file,
    // mirroring `crates/core/src/queue.rs`'s own `seed()` test helper.
    // `feathermail-core`'s `Core.db` is `pub(crate)`, unreachable from
    // this crate, so this crate cannot use `Core` itself to seed rows
    // outside its public command surface; a direct `rusqlite` connection
    // to the same WAL-mode file is the same trick `Core::open`'s own
    // doc comment describes as the supported way to have more than one
    // handle on one file. The schema is created first by a real
    // `Core::open` (never re-declared by hand here), then these tests
    // only ever seed rows using this crate's own dev-dependency on
    // `rusqlite` (already pinned once, workspace-wide, in the root
    // `Cargo.toml`) -- not by inventing a second copy of the schema.
    /// A `last_sync_at` far enough in the future that [`next_sync`] never
    /// calls a folder stamped with it due, regardless of which clock a
    /// test uses ([`SystemClock`]'s real epoch seconds, or [`FakeClock`]'s
    /// `1_700_000_000`-ish fixtures) or how much simulated/real time a
    /// test lets pass. Used by the seed helpers below to keep this
    /// module's queue-focused tests -- written before T-078 (b) added
    /// folder-sync-on-idle -- exercising only what they say they exercise:
    /// `Core::tick_for_account` and the operation queue, not an incidental
    /// folder sync pass that would otherwise fire because a freshly seeded
    /// folder has never "successfully synced" (`last_synced_at: None` is
    /// exactly the case `next_sync` treats as due immediately).
    const FAR_FUTURE_SYNC_AT: i64 = 4_102_444_800; // 2100-01-01T00:00:00Z

    /// Sets `folder_id`'s `sync_state.last_sync_at` directly (upserting the
    /// row if it doesn't exist yet) -- a second, separate
    /// `rusqlite::Connection` onto the same on-disk file, exactly like
    /// every other raw-SQL seed helper in this module. Used two ways below:
    /// with [`FAR_FUTURE_SYNC_AT`] to keep a folder permanently "not due"
    /// for the queue-focused tests that predate T-078 (b), and with a
    /// concrete recent timestamp to give a T-078 (b) test control over
    /// exactly how many seconds [`next_sync`] reports until that folder
    /// comes due (see `combined_idle_wait_after_a_folder_hint_and_a_queue_retry_takes_the_shorter_one_not_the_sum`).
    fn stamp_folder_last_sync_at(
        path: &std::path::Path,
        account: &str,
        folder_id: &str,
        last_sync_at: i64,
    ) {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute(
            "INSERT INTO sync_state (account_id, folder_id, last_sync_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(account_id, folder_id) DO UPDATE SET last_sync_at = excluded.last_sync_at",
            rusqlite::params![account, folder_id, last_sync_at],
        )
        .unwrap();
    }

    /// Stamps `folder_id` as already synced ([`FAR_FUTURE_SYNC_AT`]) so
    /// T-078 (b)'s folder-sync-on-idle check in [`sync_one_due_folder`]
    /// never fires for it -- see that constant's doc comment.
    fn mark_folder_already_synced(path: &std::path::Path, account: &str, folder_id: &str) {
        stamp_folder_last_sync_at(path, account, folder_id, FAR_FUTURE_SYNC_AT);
    }

    fn seed_account_folder_thread(path: &std::path::Path, account: &str) {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute(
            "INSERT INTO accounts (id, name, email, provider, status, download_policy, created_at, updated_at)
             VALUES (?1, ?1, 'a@example.com', 'generic', 'synced', 'recent', 0, 0)",
            rusqlite::params![account],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO folders (id, account_id, name, kind) VALUES ('inbox', ?1, 'Inbox', 'inbox')",
            rusqlite::params![account],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO threads (id, account_id, folder_id, subject, snippet, date, unread)
             VALUES ('t1', ?1, 'inbox', 'Hello', 'Hi', 0, 1)",
            rusqlite::params![account],
        )
        .unwrap();
        drop(conn);
        mark_folder_already_synced(path, account, "inbox");
    }

    /// Like [`seed_account_folder_thread`] but with per-account row ids,
    /// so a profile can hold two accounts at once.
    fn seed_account_with_own_rows(path: &std::path::Path, account: &str) {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute(
            "INSERT INTO accounts (id, name, email, provider, status, download_policy, created_at, updated_at)
             VALUES (?1, ?1, ?1 || '@example.com', 'generic', 'synced', 'recent', 0, 0)",
            rusqlite::params![account],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO folders (id, account_id, name, kind) VALUES ('inbox-' || ?1, ?1, 'Inbox', 'inbox')",
            rusqlite::params![account],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO threads (id, account_id, folder_id, subject, snippet, date, unread)
             VALUES ('t-' || ?1, ?1, 'inbox-' || ?1, 'Hello', 'Hi', 0, 1)",
            rusqlite::params![account],
        )
        .unwrap();
        drop(conn);
        mark_folder_already_synced(path, account, &format!("inbox-{account}"));
    }

    /// Seeds one account with one folder that has a resolvable `remote_id`
    /// (so `Core::remote_folder` succeeds, unlike every folder
    /// `seed_account_folder_thread`/`seed_account_with_own_rows` seed) and
    /// deliberately no `sync_state` row at all -- `last_synced_at: None`,
    /// which `next_sync` treats as due right now. This is what lets the
    /// T-078 (b) tests below actually reach [`MailSession::sync_one_folder`],
    /// unlike every queue-focused test above, which stops at
    /// `Core::remote_folder` and never gets there.
    fn seed_account_with_due_folder(
        path: &std::path::Path,
        account: &str,
        folder_id: &str,
        remote_id: &str,
    ) {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute(
            "INSERT INTO accounts (id, name, email, provider, status, download_policy, created_at, updated_at)
             VALUES (?1, ?1, 'a@example.com', 'generic', 'synced', 'recent', 0, 0)",
            rusqlite::params![account],
        )
        .unwrap();
        add_due_folder(path, account, folder_id, remote_id);
    }

    /// Adds one more resolvable, never-synced folder to an account that
    /// already exists -- see [`seed_account_with_due_folder`] for why
    /// "resolvable" and "never synced" both matter. Kept separate from
    /// that function so a test can put two due folders on one account
    /// without violating `accounts`' primary key.
    fn add_due_folder(path: &std::path::Path, account: &str, folder_id: &str, remote_id: &str) {
        add_resolvable_folder(path, account, folder_id, remote_id, "inbox");
    }

    /// A header row the warm-up can actually fetch: `provider_uid` set,
    /// `body_path` still NULL. T-119's "new mail" is this shape -- sync
    /// writes the header, the body is what must then land without a click.
    fn seed_fetchable_header(
        path: &std::path::Path,
        account: &str,
        folder_id: &str,
        message_id: &str,
        date: i64,
    ) {
        let conn = rusqlite::Connection::open(path).unwrap();
        let thread_id = format!("t-{message_id}");
        conn.execute(
            "INSERT INTO threads (id, account_id, folder_id, subject, snippet, date, unread) \
             VALUES (?1, ?2, ?3, 'subj', '', ?4, 1)",
            rusqlite::params![thread_id, account, folder_id, date],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (id, account_id, thread_id, folder_id, date, subject, provider_uid) \
             VALUES (?1, ?2, ?3, ?4, ?5, 'subj', 1)",
            rusqlite::params![message_id, account, thread_id, folder_id, date],
        )
        .unwrap();
    }

    /// Like [`add_due_folder`] with an explicit `folders.kind`, so a test
    /// can put a resolvable non-Inbox mailbox next to Inbox and then
    /// point [`Viewport::open_folder`] at it (T-089 remainder).
    fn add_resolvable_folder(
        path: &std::path::Path,
        account: &str,
        folder_id: &str,
        remote_id: &str,
        kind: &str,
    ) {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute(
            "INSERT INTO folders (id, account_id, name, kind, remote_id) VALUES (?2, ?1, ?2, ?4, ?3)",
            rusqlite::params![account, folder_id, remote_id, kind],
        )
        .unwrap();
    }

    /// Like [`seed_account_with_due_folder`], but with a non-`inbox`
    /// `kind` (T-091's own tests use this deliberately): a folder
    /// [`FolderRole`] tags as `Inbox` would route `run`'s idle branch
    /// through `watch_inbox_for_push`/`MailSession::idle_once` instead of
    /// `sync_one_due_folder`/`MailSession::sync_one_folder` once the
    /// first (due-immediately) attempt is done and the folder starts
    /// serving a D33 backoff -- not the call [`AuthFailProvider`] below
    /// exists to exercise session identity through.
    fn seed_account_with_due_custom_folder(
        path: &std::path::Path,
        account: &str,
        folder_id: &str,
        remote_id: &str,
    ) {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute(
            "INSERT INTO accounts (id, name, email, provider, status, download_policy, created_at, updated_at)
             VALUES (?1, ?1, 'a@example.com', 'generic', 'synced', 'recent', 0, 0)",
            rusqlite::params![account],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO folders (id, account_id, name, kind, remote_id) VALUES (?2, ?1, ?2, 'custom', ?3)",
            rusqlite::params![account, folder_id, remote_id],
        )
        .unwrap();
    }

    /// Adds one folder that is due right now but has **no** `remote_id` --
    /// T-077's placeholder case, and exactly what T-084's refused-by-the-
    /// server folder looks like in the database. `Core::remote_folder`
    /// cannot resolve anything to `SELECT` for it, so `sync_one_due_folder`
    /// never reaches the wire; what it must still do is record the attempt
    /// (see `an_unresolvable_folder_still_records_its_attempt_instead_of_
    /// coming_due_forever`).
    fn add_unresolvable_folder(path: &std::path::Path, account: &str, folder_id: &str) {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute(
            "INSERT INTO folders (id, account_id, name, kind, remote_id) VALUES (?2, ?1, ?2, 'custom', NULL)",
            rusqlite::params![account, folder_id],
        )
        .unwrap();
    }

    #[test]
    fn connection_bootstrap_adopts_the_placeholder_inbox_before_the_first_sync() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        Core::open(&path).unwrap();
        seed_account_folder_thread(&path, "acc1");

        let mut core = Core::open(&path).unwrap();
        let calls = Arc::new(AtomicU32::new(0));
        let mut session = FolderDiscoverySession {
            calls: Arc::clone(&calls),
        };
        let account = AccountId("acc1".into());

        bootstrap_folders_if_needed(&mut core, &account, &mut session).unwrap();

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a placeholder Inbox must trigger LIST"
        );
        assert_eq!(
            core.remote_folder(&account, "inbox").unwrap(),
            "INBOX",
            "the existing Inbox row must be adopted, so the first scheduled sync can SELECT it"
        );

        bootstrap_folders_if_needed(&mut core, &account, &mut session).unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a resolved Inbox must not LIST again on every reconnect"
        );
    }

    /// Reads back `sync_state.last_attempt_at`/`consecutive_failures` for
    /// one folder -- what `Core::record_sync_attempt` actually wrote,
    /// checked independently of the [`SyncEvent`] the worker also emits,
    /// so a test can tell "the event fired" and "the attempt was durably
    /// recorded" apart (see this module's T-078 (b) doc comment on why
    /// that recording is what stops `next_sync` reporting the same folder
    /// due forever).
    fn sync_state_attempt(path: &std::path::Path, account: &str, folder_id: &str) -> (i64, i64) {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.query_row(
            "SELECT last_attempt_at, consecutive_failures FROM sync_state \
             WHERE account_id = ?1 AND folder_id = ?2",
            rusqlite::params![account, folder_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
    }

    fn operation_status(path: &std::path::Path) -> (String, i64) {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.query_row(
            "SELECT status, retry_count FROM operations WHERE target_id = 't1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
    }

    fn mark_operation_running(path: &std::path::Path) {
        let conn = rusqlite::Connection::open(path).unwrap();
        let n = conn
            .execute(
                "UPDATE operations SET status = 'running' WHERE target_id = 't1'",
                [],
            )
            .unwrap();
        assert_eq!(n, 1, "seed must have created exactly one operation to flip");
    }

    fn archive_t1(path: &std::path::Path, account: &str) {
        archive_thread(path, account, "t1");
    }

    fn archive_thread(path: &std::path::Path, account: &str, thread_id: &str) {
        let mut core = Core::open(path).unwrap();
        core.dispatch(Command::Archive {
            account_id: AccountId(account.into()),
            thread_ids: vec![ThreadId(thread_id.into())],
        })
        .unwrap();
    }

    fn wait_until(mut condition: impl FnMut() -> bool, timeout: Duration) {
        let start = Instant::now();
        while !condition() {
            assert!(
                start.elapsed() < timeout,
                "condition not met within {timeout:?}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    // --- T-092: `fts_pending` backlog seeding + inspection helpers ---
    //
    // `messages.account_id`/`thread_id`/`folder_id` are all `NOT NULL
    // REFERENCES ...` (`foreign_keys = ON`, D13), so seeding a pending
    // message means seeding one real account/folder/thread underneath it
    // too -- same shape as `seed_account_folder_thread` above, just with
    // `fts_pending` rows added on top (mirroring `CoreSyncStore::
    // upsert_one`'s new-message branch, which enqueues every message it
    // inserts).

    /// One account, one folder, `count` messages each on their own thread
    /// with a distinct, greppable subject (`"t092 subject <n>"`) and no
    /// `body_path` -- `index_one` (`crates/core/src/search.rs`) indexes
    /// subject/sender/recipients/attachments/labels regardless of whether
    /// a body is cached, so this is enough to prove a message became
    /// findable without needing a body fixture on disk. Every message is
    /// left enqueued in `fts_pending`, exactly as if
    /// `CoreSyncStore::upsert_one` had just synced it. Row-at-a-time
    /// (`count` individual `INSERT`s) -- fine for the small counts this
    /// helper is used for; see [`seed_account_with_a_bulk_pending_backlog`]
    /// for the bulk version large backlog tests need instead.
    fn seed_account_with_pending_messages(path: &std::path::Path, account: &str, count: usize) {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute(
            "INSERT INTO accounts (id, name, email, provider, status, download_policy, created_at, updated_at)
             VALUES (?1, ?1, 'a@example.com', 'generic', 'synced', 'recent', 0, 0)",
            rusqlite::params![account],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO folders (id, account_id, name, kind) VALUES ('inbox', ?1, 'Inbox', 'inbox')",
            rusqlite::params![account],
        )
        .unwrap();
        for i in 0..count {
            let thread_id = format!("t{i}");
            let msg_id = format!("m{i}");
            let subject = format!("t092 subject {i}");
            conn.execute(
                "INSERT INTO threads (id, account_id, folder_id, subject, snippet, date, unread) \
                 VALUES (?1, ?2, 'inbox', ?3, '', ?4, 1)",
                rusqlite::params![thread_id, account, subject, i as i64],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO messages (id, account_id, thread_id, folder_id, date, subject) \
                 VALUES (?1, ?2, ?3, 'inbox', ?4, ?5)",
                rusqlite::params![msg_id, account, thread_id, i as i64, subject],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO fts_pending (message_id, queued_at) VALUES (?1, ?2)",
                rusqlite::params![msg_id, i as i64],
            )
            .unwrap();
        }
        drop(conn);
        mark_folder_already_synced(path, account, "inbox");
    }

    /// Like [`seed_account_with_pending_messages`], but `count` messages
    /// share a single thread and are inserted via one `INSERT ... SELECT`
    /// per table (a `WITH RECURSIVE` row generator) instead of `count`
    /// individual round trips. Needed for T-092's shutdown-mid-backlog
    /// test, which wants a backlog large enough that draining all of it
    /// is measurably slower than shutting down -- without spending that
    /// same wall-clock time just seeding rows before the test's own timing
    /// assertion even starts. Subjects are not individually distinctive
    /// here (`"bulk subject <n>"`) -- this helper's callers only ever
    /// check `fts_pending` row counts, never search for one particular
    /// message.
    fn seed_account_with_a_bulk_pending_backlog(
        path: &std::path::Path,
        account: &str,
        count: usize,
    ) {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute(
            "INSERT INTO accounts (id, name, email, provider, status, download_policy, created_at, updated_at)
             VALUES (?1, ?1, 'a@example.com', 'generic', 'synced', 'recent', 0, 0)",
            rusqlite::params![account],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO folders (id, account_id, name, kind) VALUES ('inbox', ?1, 'Inbox', 'inbox')",
            rusqlite::params![account],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO threads (id, account_id, folder_id, subject, snippet, date, unread) \
             VALUES ('t-bulk', ?1, 'inbox', 'bulk', '', 0, 1)",
            rusqlite::params![account],
        )
        .unwrap();
        conn.execute(
            "WITH RECURSIVE seq(n) AS ( \
                 SELECT 0 UNION ALL SELECT n + 1 FROM seq WHERE n + 1 < ?2 \
             ) \
             INSERT INTO messages (id, account_id, thread_id, folder_id, date, subject) \
             SELECT 'm-bulk-' || n, ?1, 't-bulk', 'inbox', n, 'bulk subject ' || n FROM seq",
            rusqlite::params![account, count as i64],
        )
        .unwrap();
        conn.execute(
            "WITH RECURSIVE seq(n) AS ( \
                 SELECT 0 UNION ALL SELECT n + 1 FROM seq WHERE n + 1 < ?1 \
             ) \
             INSERT INTO fts_pending (message_id, queued_at) \
             SELECT 'm-bulk-' || n, n FROM seq",
            rusqlite::params![count as i64],
        )
        .unwrap();
        drop(conn);
        mark_folder_already_synced(path, account, "inbox");
    }

    /// Current `fts_pending` row count -- a second, raw `rusqlite`
    /// connection onto the same on-disk file, same idiom as every other
    /// inspection helper in this module (e.g. [`operation_status`]).
    fn fts_pending_count(path: &std::path::Path) -> i64 {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.query_row("SELECT COUNT(*) FROM fts_pending", [], |row| row.get(0))
            .unwrap()
    }

    /// Whether `messages_fts` has at least one row matching `term` via a
    /// real FTS5 `MATCH` query -- the same query shape
    /// `Core::search`/`build_search_sql` issues
    /// (`messages_fts MATCH ?`, see `crates/core/src/search.rs`), just run
    /// directly rather than through `feathermail_core::Core::search`
    /// itself. `Core::search` takes a `feathermail_search::SearchPlan`,
    /// and `feathermail-search` is not a dependency of `feathermail-
    /// service` (adding it would add a new edge to `Cargo.lock`, which
    /// this ticket's stated perimeter puts out of bounds -- see the
    /// report). Querying `messages_fts` directly proves the same fact
    /// `Core::search` would have reported (`pending_index` reaching zero
    /// and the term becoming matchable), through the identical index and
    /// an equivalent `MATCH` query, without that dependency edge.
    fn messages_fts_matches(path: &std::path::Path, term: &str) -> bool {
        let conn = rusqlite::Connection::open(path).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH ?1",
                rusqlite::params![term],
                |row| row.get(0),
            )
            .unwrap();
        count > 0
    }

    /// Core acceptance case: a command dispatched on a real on-disk
    /// profile reaches a fake provider and is `acked` in the `operations`
    /// table -- entirely through the background worker. The test never
    /// calls `Core::tick` itself.
    #[test]
    fn dispatched_operation_reaches_fake_provider_and_is_acked_without_the_test_ticking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        } // creates the schema
        seed_account_folder_thread(&path, "acc1");
        archive_t1(&path, "acc1");

        let (factory, applies, connects) = FakeFactory::new(0);
        let events: Arc<Mutex<Vec<SyncEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let events_for_worker = Arc::clone(&events);
        let handle = start(path.clone(), factory, move |e| {
            events_for_worker.lock().unwrap().push(e);
        });

        wait_until(
            || !events.lock().unwrap().is_empty(),
            Duration::from_secs(2),
        );
        handle.shutdown();

        assert_eq!(applies.load(Ordering::SeqCst), 1);
        assert_eq!(connects.load(Ordering::SeqCst), 1);
        let fired = events.lock().unwrap();
        assert_eq!(fired.len(), 1);
        assert!(matches!(fired[0], SyncEvent::Acked { .. }));
        let (status, _) = operation_status(&path);
        assert_eq!(status, "acked");
    }

    /// D32: a provider that keeps failing with `ApplyError::Network`
    /// drives the worker into the retry/backoff table instead of a hot
    /// loop. Proved by counters (not by reasoning about the code): the
    /// number of `apply`/`connect` calls is exactly what 3 failures then a
    /// success implies, and the whole test -- which spans backoff delays
    /// of 2s, 5s and 15s (22 real seconds if actually slept) -- completes
    /// in well under a second of real wall-clock time because the
    /// worker's own waits are driven by [`FakeClock`], not
    /// [`SystemClock`].
    #[test]
    fn network_failures_back_off_instead_of_hot_looping() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        seed_account_folder_thread(&path, "acc1");
        archive_t1(&path, "acc1");

        let (factory, applies, connects) = FakeFactory::new(3);
        let events: Arc<Mutex<Vec<SyncEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let events_for_worker = Arc::clone(&events);
        let wall_clock_start = Instant::now();
        let handle = start_with_clock(
            path.clone(),
            factory,
            move |e| events_for_worker.lock().unwrap().push(e),
            FakeClock::new(1_700_000_000),
            FixedPower(false),
        );

        wait_until(
            || {
                events
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|e| matches!(e, SyncEvent::Acked { .. }))
            },
            Duration::from_secs(2),
        );
        let elapsed = wall_clock_start.elapsed();
        handle.shutdown();

        // 3 failing attempts + 1 successful attempt.
        assert_eq!(applies.load(Ordering::SeqCst), 4);
        // The same session is reused across the retries -- one connect,
        // not one per attempt.
        assert_eq!(connects.load(Ordering::SeqCst), 1);
        assert_eq!(operation_status(&path).0, "acked");
        assert!(
            elapsed < Duration::from_secs(2),
            "backoff waits must be simulated, not actually slept: took {elapsed:?}"
        );
    }

    /// `shutdown` must return promptly and must not leak the worker
    /// thread, even against an empty profile where the worker is sitting
    /// in its longest (15-minute) idle wait.
    #[test]
    fn shutdown_is_fast_and_joins_the_thread() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        } // empty profile, no accounts at all

        let (factory, _applies, _connects) = FakeFactory::new(0);
        let handle = start(path, factory, |_event| {});

        let started = Instant::now();
        handle.shutdown();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "shutdown must not wait out the worker's idle timeout"
        );
    }

    /// D31: an operation left `running` by a simulated crash (the
    /// process died mid-`apply`, so `finish` never ran) is still delivered
    /// once the worker starts, because `Core::open`'s own
    /// `recover_inflight` -- and this worker's own explicit call to it --
    /// puts it back to `pending` before the loop ever runs.
    #[test]
    fn worker_started_after_a_crash_still_delivers_the_stuck_operation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        seed_account_folder_thread(&path, "acc1");
        archive_t1(&path, "acc1");
        mark_operation_running(&path);
        assert_eq!(operation_status(&path).0, "running");

        let (factory, applies, _connects) = FakeFactory::new(0);
        let events: Arc<Mutex<Vec<SyncEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let events_for_worker = Arc::clone(&events);
        let handle = start(path.clone(), factory, move |e| {
            events_for_worker.lock().unwrap().push(e);
        });

        wait_until(
            || !events.lock().unwrap().is_empty(),
            Duration::from_secs(2),
        );
        handle.shutdown();

        assert_eq!(applies.load(Ordering::SeqCst), 1);
        assert!(matches!(events.lock().unwrap()[0], SyncEvent::Acked { .. }));
        assert_eq!(operation_status(&path).0, "acked");
    }

    /// Two accounts, one of them quiet. The quiet account's turn must end
    /// the moment `tick_for_account` reports `Idle` for it -- not park the
    /// worker on the idle poll -- or the other account's queued archive
    /// would sit unsent for up to `MAX_IDLE_POLL_SECS` (15 minutes). Runs
    /// on the real `SystemClock` precisely so that a regression here shows
    /// up as this test timing out rather than as a fake clock skipping
    /// ahead.
    #[test]
    fn a_quiet_account_does_not_park_the_worker_while_another_has_work() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        seed_account_with_own_rows(&path, "aaa-quiet");
        seed_account_with_own_rows(&path, "zzz-busy");

        let mut core = Core::open(&path).unwrap();
        core.dispatch(Command::Archive {
            account_id: AccountId("zzz-busy".into()),
            thread_ids: vec![ThreadId("t-zzz-busy".into())],
        })
        .unwrap();
        drop(core);

        let factory = PerAccountFactory::default();
        let applies = Arc::clone(&factory.applies);
        let handle = start(&path, factory, |_| {});

        wait_until(
            || !applies.lock().unwrap().is_empty(),
            Duration::from_secs(5),
        );
        handle.shutdown();

        // Every apply that happened went out on its own account's
        // connection, and the busy account's operation is the one that
        // ran even though the quiet account may well have had the first
        // turn in the round-robin.
        let seen = applies.lock().unwrap().clone();
        assert!(
            seen.iter()
                .all(|(account, target)| target == &format!("t-{account}")),
            "an operation was applied on another account's connection: {seen:?}"
        );
        assert!(
            seen.iter().any(|(account, _)| account == "zzz-busy"),
            "the busy account's operation never reached a provider: {seen:?}"
        );
    }

    /// Connect backoff is per account. Two unreachable accounts sit ahead
    /// of a healthy one in the round-robin (`list_accounts` orders by
    /// `created_at`, then id, and the seed gives all three the same
    /// `created_at`). A single global failure counter would charge their
    /// failures to the same D32 clock and make the healthy account wait
    /// out 2s + 5s of somebody else's backoff before its own queued
    /// archive was even looked at. Real `SystemClock` on purpose: a
    /// regression here shows up as this test running out of its budget.
    #[test]
    fn one_unreachable_account_does_not_delay_a_healthy_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        for account in ["aaa-broken", "bbb-broken", "zzz-healthy"] {
            seed_account_with_own_rows(&path, account);
        }

        let mut core = Core::open(&path).unwrap();
        core.dispatch(Command::Archive {
            account_id: AccountId("zzz-healthy".into()),
            thread_ids: vec![ThreadId("t-zzz-healthy".into())],
        })
        .unwrap();
        drop(core);

        let factory = SelectiveFactory {
            unreachable: vec!["aaa-broken".into(), "bbb-broken".into()],
            attempts: Arc::new(Mutex::new(Vec::new())),
            applies: Arc::new(Mutex::new(Vec::new())),
        };
        let attempts = Arc::clone(&factory.attempts);
        let applies = Arc::clone(&factory.applies);
        let handle = start(&path, factory, |_| {});

        wait_until(
            || !applies.lock().unwrap().is_empty(),
            Duration::from_secs(2),
        );
        handle.shutdown();

        assert_eq!(
            applies.lock().unwrap().clone(),
            vec![("zzz-healthy".to_string(), "t-zzz-healthy".to_string())]
        );
        // And each unreachable account was tried once, not once per lap:
        // its own backoff deadline has not come round yet.
        let tried = attempts.lock().unwrap().clone();
        for broken in ["aaa-broken", "bbb-broken"] {
            assert_eq!(
                tried.iter().filter(|id| id.as_str() == broken).count(),
                1,
                "{broken} was retried before its own backoff was due: {tried:?}"
            );
        }
    }

    /// T-080 end-to-end: a `FetchBody` sent through a real [`SyncHandle`]
    /// reaches [`MailSession::open_one_body`] on the worker thread and
    /// comes back as a [`SyncEvent::BodyReady`] with `ok: true`, naming
    /// the same message id that was asked for -- proof the whole wire,
    /// not just one function in isolation, is connected: `SyncHandle::
    /// fetch_body` -> `WorkerCommand::FetchBody` -> `run`'s pending-fetch
    /// check -> `MailSession::open_one_body` -> `SyncEvent::BodyReady`.
    #[test]
    fn fetch_body_serves_a_pending_fetch_and_emits_body_ready() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        seed_account_folder_thread(&path, "acc1");

        let body_calls = Arc::new(Mutex::new(Vec::new()));
        let factory = BodyFetchFactory {
            unreachable: vec![],
            fail_body: false,
            connects: Arc::new(Mutex::new(Vec::new())),
            body_calls: Arc::clone(&body_calls),
            attachment_calls: Arc::new(Mutex::new(Vec::new())),
        };
        let events: Arc<Mutex<Vec<SyncEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let events_for_worker = Arc::clone(&events);
        let handle = start(path.clone(), factory, move |e| {
            events_for_worker.lock().unwrap().push(e);
        });

        let message_id = MessageId("m1".into());
        handle.fetch_body(AccountId("acc1".into()), message_id.clone());

        wait_until(
            || {
                events
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|e| matches!(e, SyncEvent::BodyReady { .. }))
            },
            Duration::from_secs(2),
        );
        handle.shutdown();

        let fired = events.lock().unwrap();
        let body_ready = fired.iter().find_map(|e| match e {
            SyncEvent::BodyReady { message_id, ok } => Some((message_id.clone(), *ok)),
            _ => None,
        });
        assert_eq!(
            body_ready,
            Some((message_id.clone(), true)),
            "expected exactly one BodyReady{{ ok: true }} for m1, got: {fired:?}"
        );
        assert_eq!(
            body_calls.lock().unwrap().clone(),
            vec![("acc1".to_string(), message_id)],
            "open_one_body must have been called on the acc1 session with the requested id"
        );
    }

    /// T-043: the attachment equivalent of the body-fetch route above.
    /// The worker must invoke the session's Core-backed download door and
    /// return only the attachment id plus completion status -- never a
    /// cache path or payload (D11/D14).
    #[test]
    fn fetch_attachment_serves_a_pending_fetch_and_emits_attachment_ready() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        seed_account_folder_thread(&path, "acc1");

        let attachment_calls = Arc::new(Mutex::new(Vec::new()));
        let factory = BodyFetchFactory {
            unreachable: vec![],
            fail_body: false,
            connects: Arc::new(Mutex::new(Vec::new())),
            body_calls: Arc::new(Mutex::new(Vec::new())),
            attachment_calls: Arc::clone(&attachment_calls),
        };
        let events: Arc<Mutex<Vec<SyncEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let events_for_worker = Arc::clone(&events);
        let handle = start(path.clone(), factory, move |event| {
            events_for_worker.lock().unwrap().push(event);
        });

        let attachment_id = AttachmentId("attachment:m1:0".into());
        handle.fetch_attachment(AccountId("acc1".into()), attachment_id.clone());

        wait_until(
            || {
                events
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|event| matches!(event, SyncEvent::AttachmentReady { .. }))
            },
            Duration::from_secs(2),
        );
        handle.shutdown();

        let fired = events.lock().unwrap();
        let attachment_ready = fired.iter().find_map(|event| match event {
            SyncEvent::AttachmentReady { attachment_id, ok } => Some((attachment_id.clone(), *ok)),
            _ => None,
        });
        assert_eq!(
            attachment_ready,
            Some((attachment_id.clone(), true)),
            "expected exactly one AttachmentReady{{ ok: true }} for the requested id, got: {fired:?}"
        );
        assert_eq!(
            attachment_calls.lock().unwrap().clone(),
            vec![("acc1".to_string(), attachment_id)],
            "download_one_attachment must be called on the requested account session"
        );
    }

    /// T-080: a `FetchBody` for an account that cannot connect at all must
    /// still resolve -- with `ok: false` -- rather than leave the shell's
    /// reading pane on "Loading" forever. `BodyReady` never carries the
    /// bytes (D14), so `ok` is the only signal the shell gets back; if a
    /// connect failure did not produce one, there would be no way to tell
    /// "still loading" apart from "never coming".
    #[test]
    fn fetch_body_for_an_unreachable_account_resolves_with_ok_false() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        seed_account_folder_thread(&path, "acc1");

        let factory = BodyFetchFactory {
            unreachable: vec!["acc1".into()],
            fail_body: false,
            connects: Arc::new(Mutex::new(Vec::new())),
            body_calls: Arc::new(Mutex::new(Vec::new())),
            attachment_calls: Arc::new(Mutex::new(Vec::new())),
        };
        let events: Arc<Mutex<Vec<SyncEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let events_for_worker = Arc::clone(&events);
        let handle = start(path.clone(), factory, move |e| {
            events_for_worker.lock().unwrap().push(e);
        });

        let message_id = MessageId("m1".into());
        handle.fetch_body(AccountId("acc1".into()), message_id.clone());

        wait_until(
            || {
                events
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|e| matches!(e, SyncEvent::BodyReady { .. }))
            },
            Duration::from_secs(2),
        );
        handle.shutdown();

        let fired = events.lock().unwrap();
        let body_ready = fired.iter().find_map(|e| match e {
            SyncEvent::BodyReady { message_id, ok } => Some((message_id.clone(), *ok)),
            _ => None,
        });
        assert_eq!(
            body_ready,
            Some((message_id, false)),
            "a connect failure must resolve the pending fetch with ok: false, not silence: {fired:?}"
        );
        assert!(
            fired
                .iter()
                .any(|e| matches!(e, SyncEvent::ConnectFailed { .. })),
            "the connect attempt itself must still be reported like any other: {fired:?}"
        );
    }

    /// Direct unit test of [`wait_for_shutdown`] itself: [`WorkerClock::
    /// wait`] consumes at most one command off `rx`, so if that command is
    /// a `FetchBody` and this function only recognized `Shutdown`, the
    /// fetch would simply vanish -- `recv_timeout` does not put a message
    /// back. Proves it lands in `pending_fetches` instead.
    #[test]
    fn wait_for_shutdown_does_not_drop_a_fetch_body_sent_during_a_blocking_wait() {
        let (tx, rx) = mpsc::channel();
        let account_id = AccountId("acc1".into());
        let message_id = MessageId("m1".into());
        tx.send(WorkerCommand::FetchBody {
            account_id: account_id.clone(),
            message_id: message_id.clone(),
        })
        .unwrap();

        let pending_fetches: RefCell<VecDeque<PendingFetch>> = RefCell::new(VecDeque::new());
        let shutdown = wait_for_shutdown(&rx, &SystemClock, 5, &pending_fetches);

        assert!(!shutdown, "a FetchBody is not a Shutdown");
        assert_eq!(
            pending_fetches.into_inner(),
            VecDeque::from([PendingFetch::Body {
                account_id,
                message_id,
            }]),
            "the FetchBody consumed off rx by clock.wait must not simply vanish"
        );
    }

    /// D11: the GTK shell holds its `SyncHandle` for the lifetime of the
    /// window, so the handle is dropped on the GTK thread while the window
    /// closes. If `Drop` joined the worker, that close would block for as
    /// long as whatever socket call the worker happened to be inside --
    /// eight seconds per operation with `feathermail_providers`'s wire
    /// timeout. Here the worker is parked inside `connect` and cannot
    /// possibly finish; dropping the handle must still return at once.
    #[test]
    fn dropping_the_handle_does_not_wait_for_a_worker_stuck_on_the_network() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        seed_account_folder_thread(&path, "acct");

        let entered = Arc::new(AtomicU32::new(0));
        let left = Arc::new(AtomicU32::new(0));
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let handle = start(
            &path,
            BlockingFactory {
                entered: Arc::clone(&entered),
                release: Arc::clone(&release),
                left: Arc::clone(&left),
            },
            |_| {},
        );
        wait_until(
            || entered.load(Ordering::SeqCst) > 0,
            Duration::from_secs(5),
        );

        let started = Instant::now();
        drop(handle);
        let took = started.elapsed();
        assert!(
            took < Duration::from_millis(250),
            "dropping the handle waited {took:?} on a worker parked in connect"
        );

        // Let the parked worker go and wait for it to actually leave, so
        // this test does not hand the rest of the suite a live thread
        // holding a database inside a tempdir that is about to vanish.
        {
            let (lock, cv) = &*release;
            *lock.lock().unwrap() = true;
            cv.notify_all();
        }
        wait_until(|| left.load(Ordering::SeqCst) > 0, Duration::from_secs(5));
    }

    /// T-067/D11: startup creates a non-blocking handle, but a deferred
    /// worker cannot enter a real provider connect until the shell has
    /// explicitly crossed its first-frame boundary. `BlockingFactory` models
    /// a socket connect that never returns until the test releases it.
    ///
    /// `start_deferred` itself runs in a bounded helper thread. A regression
    /// that accidentally performs connect synchronously therefore fails this
    /// test by name, rather than parking the whole test runner inside the
    /// blocking double before it reaches cleanup.
    #[test]
    fn deferred_start_waits_for_activation_before_a_provider_connect() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        seed_account_folder_thread(&path, "acct");

        let entered = Arc::new(AtomicU32::new(0));
        let left = Arc::new(AtomicU32::new(0));
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let (returned_tx, returned_rx) = mpsc::channel();
        let start_path = path.clone();
        let start_entered = Arc::clone(&entered);
        let start_left = Arc::clone(&left);
        let start_release = Arc::clone(&release);
        let starter = std::thread::spawn(move || {
            let started = Instant::now();
            let handle = start_deferred(
                start_path,
                BlockingFactory {
                    entered: start_entered,
                    release: start_release,
                    left: start_left,
                },
                |_| {},
            );
            let _ = returned_tx.send((handle, started.elapsed()));
        });
        let (mut handle, start_elapsed) = match returned_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(returned) => returned,
            Err(_) => {
                // A mutant that connected on this helper thread can now
                // leave the blocking double and cannot wedge the suite.
                let (lock, cv) = &*release;
                *lock.lock().unwrap() = true;
                cv.notify_all();
                panic!(
                    "start_deferred() did not return its handle within 1s; it must not connect synchronously"
                );
            }
        };
        starter
            .join()
            .expect("the non-blocking start helper must not panic");
        assert!(
            start_elapsed < Duration::from_millis(250),
            "start_deferred() waited for a provider connect instead of returning its handle"
        );
        assert_eq!(
            entered.load(Ordering::SeqCst),
            0,
            "a deferred worker entered provider connect before activation"
        );
        assert!(
            handle.activate(),
            "the first activation must release the deferred worker"
        );
        assert!(
            !handle.activate(),
            "a second activation must not create another startup path"
        );
        wait_until(
            || entered.load(Ordering::SeqCst) > 0,
            Duration::from_secs(5),
        );

        {
            let (lock, cv) = &*release;
            *lock.lock().unwrap() = true;
            cv.notify_all();
        }
        handle.shutdown();
        assert_eq!(
            left.load(Ordering::SeqCst),
            1,
            "the released worker must leave its blocked connect before the temp profile disappears"
        );
    }

    /// The map callback may never run when the application is closed during
    /// startup. In that case a dormant worker must still join promptly,
    /// without opening the profile or waiting for a provider connection.
    #[test]
    fn deferred_shutdown_before_activation_joins_without_a_provider_connect() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        let entered = Arc::new(AtomicU32::new(0));
        let handle = start_deferred(
            &path,
            BlockingFactory {
                entered: Arc::clone(&entered),
                release: Arc::new((Mutex::new(false), Condvar::new())),
                left: Arc::new(AtomicU32::new(0)),
            },
            |_| {},
        );
        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let started = Instant::now();
            handle.shutdown();
            let _ = done_tx.send(started.elapsed());
        });
        let elapsed = done_rx.recv_timeout(Duration::from_secs(1)).expect(
            "shutdown before deferred activation must not wait for window map or provider connect",
        );
        assert!(
            elapsed < Duration::from_millis(250),
            "deferred shutdown before activation took {elapsed:?}"
        );
        assert_eq!(
            entered.load(Ordering::SeqCst),
            0,
            "a shutdown-before-map worker must never enter provider connect"
        );
    }

    // --- T-089: IMAP `IDLE` on the focused-or-Inbox folder, replacing a
    // plain timed sleep when an account has no queue retry pending -- see
    // `watch_inbox_for_push`'s and this module's own top-level doc
    // comments for the full design. `IdleBlockingProvider`/
    // `IdleBlockingFactory` (declared above, next to `BlockingFactory`)
    // back the blocking tests below.

    /// Fork #1: a command sent *while* the worker is parked inside
    /// `MailSession::idle_once` must not be dropped and must not sit
    /// until the full `IDLE` ceiling elapses -- the same class of bug
    /// `sync_one_due_folder_does_not_drop_a_fetch_body_sent_during_the_sync_pass`
    /// and `wait_for_shutdown_does_not_drop_a_fetch_body_sent_during_a_blocking_wait`
    /// already guard for the other two waits this worker does. Real
    /// timing on purpose -- `IdleBlockingProvider::idle_once` genuinely
    /// blocks on a live `should_stop` poll, not a fake that returns
    /// instantly regardless -- and the ceiling handed to it (~45s, the
    /// Inbox role's own baseline interval) is large enough that "answered
    /// promptly" and "answered because the fake ignores its ceiling"
    /// cannot be confused.
    #[test]
    fn a_fetch_body_sent_during_an_idle_round_is_not_dropped_and_does_not_wait_out_the_ceiling() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        seed_account_with_due_folder(&path, "acc1", "inbox", "INBOX");
        // Real `SystemClock`, not `FakeClock`, backs this test (it needs
        // genuine wall-clock timing to prove a response is prompt, not
        // simulated) -- so, unlike `a_folder_due_soon_shortens_the_idle_
        // sleep_instead_of_waiting_out_the_full_poll`'s fixed simulated
        // epoch, this folder's `last_sync_at` must be stamped against the
        // *real* current time, or it reads as millions of seconds overdue
        // instead of "just synced, next due in ~45s".
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        stamp_folder_last_sync_at(&path, "acc1", "inbox", now);

        let idle_calls = Arc::new(AtomicU32::new(0));
        let body_calls = Arc::new(Mutex::new(Vec::new()));
        let factory = IdleBlockingFactory {
            connects: Arc::new(AtomicU32::new(0)),
            idle_calls: Arc::clone(&idle_calls),
            idle_folders: Arc::new(Mutex::new(Vec::new())),
            body_calls: Arc::clone(&body_calls),
            fail_idle_times: Arc::new(AtomicU32::new(0)),
        };
        let events: Arc<Mutex<Vec<SyncEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let events_for_worker = Arc::clone(&events);
        let handle = start(path.clone(), factory, move |e| {
            events_for_worker.lock().unwrap().push(e);
        });

        wait_until(
            || idle_calls.load(Ordering::SeqCst) > 0,
            Duration::from_secs(2),
        );

        let message_id = MessageId("m1".into());
        let sent_at = Instant::now();
        handle.fetch_body(AccountId("acc1".into()), message_id.clone());

        wait_until(
            || {
                events
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|e| matches!(e, SyncEvent::BodyReady { .. }))
            },
            Duration::from_secs(2),
        );
        let elapsed = sent_at.elapsed();
        handle.shutdown();

        assert!(
            elapsed < Duration::from_secs(2),
            "a FetchBody sent during an IDLE round with a ~45s ceiling must be served in well \
             under that ceiling, not wait it out: took {elapsed:?}"
        );
        let fired = events.lock().unwrap();
        let body_ready = fired.iter().find_map(|e| match e {
            SyncEvent::BodyReady { message_id, ok } => Some((message_id.clone(), *ok)),
            _ => None,
        });
        assert_eq!(
            body_ready,
            Some((message_id.clone(), true)),
            "expected exactly one BodyReady{{ ok: true }} for m1, got: {fired:?}"
        );
        assert_eq!(
            body_calls.lock().unwrap().clone(),
            vec![("acc1".to_string(), message_id)],
            "open_one_body must have been called with the requested id, proving the FetchBody \
             sent mid-IDLE was not simply lost"
        );
    }

    fn pending_acc(id: &str) -> AccountId {
        AccountId(id.into())
    }

    fn pending_body(account: &str, id: &str) -> PendingFetch {
        PendingFetch::Body {
            account_id: pending_acc(account),
            message_id: MessageId(id.into()),
        }
    }

    fn pending_warm(account: &str, id: &str) -> PendingFetch {
        PendingFetch::Warm {
            account_id: pending_acc(account),
            message_ids: vec![MessageId(id.into())],
        }
    }

    /// T-115: FIFO would reconnect to the warm-up's mailbox and leave the
    /// click sitting until that account's IDLE slice ended.
    #[test]
    fn a_click_outranks_a_warmup_when_picking_which_account_to_connect() {
        let queue = VecDeque::from([pending_warm("acc1", "w1"), pending_body("acc2", "c1")]);
        assert_eq!(
            preferred_pending_account(&queue).unwrap().as_str(),
            "acc2",
            "the person waiting on acc2 must jump the warm-up that opened acc1's folder"
        );
    }

    /// T-115: connected to acc1 with a click for acc2 waiting -- drop the
    /// session rather than serve acc1's warm-up (or start IDLE). Mutation:
    /// Serve(0) for the warm-up, or None, restores the letter that never
    /// loads.
    #[test]
    fn a_click_for_the_other_mailbox_drops_the_live_session() {
        let queue = VecDeque::from([pending_warm("acc1", "w1"), pending_body("acc2", "c1")]);
        assert_eq!(
            connected_fetch_action(&queue, &pending_acc("acc1")),
            ConnectedFetch::SwitchAccount
        );
        assert_eq!(
            connected_fetch_action(&queue, &pending_acc("acc2")),
            ConnectedFetch::Serve(1)
        );
    }

    /// Same-account click still outranks that account's own warm-up -- the
    /// T-024 rule, pinned so T-115's "other mailbox" branch cannot steal it.
    #[test]
    fn a_click_for_this_mailbox_still_outranks_its_own_warmup() {
        let queue = VecDeque::from([pending_warm("acc1", "w1"), pending_body("acc1", "c1")]);
        assert_eq!(
            connected_fetch_action(&queue, &pending_acc("acc1")),
            ConnectedFetch::Serve(1)
        );
    }

    /// T-115 end-to-end: two mailboxes, acc1 is in IDLE, a warm-up for
    /// acc1 and a click for acc2 land in the same drain. FIFO would
    /// reconnect to acc1, serve the warm-up, then IDLE for up to 30s with
    /// the click already in `pending_fetches` (so `should_stop` never sees
    /// it on `rx`). The click must come back in well under that slice.
    #[test]
    fn a_click_on_the_other_account_is_not_left_behind_a_warmup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        seed_account_with_due_folder(&path, "acc1", "acc1:inbox", "INBOX1");
        seed_account_with_due_folder(&path, "acc2", "acc2:inbox", "INBOX2");
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        stamp_folder_last_sync_at(&path, "acc1", "acc1:inbox", now);
        stamp_folder_last_sync_at(&path, "acc2", "acc2:inbox", now);

        let idle_folders = Arc::new(Mutex::new(Vec::new()));
        let body_calls = Arc::new(Mutex::new(Vec::new()));
        let factory = IdleBlockingFactory {
            connects: Arc::new(AtomicU32::new(0)),
            idle_calls: Arc::new(AtomicU32::new(0)),
            idle_folders: Arc::clone(&idle_folders),
            body_calls: Arc::clone(&body_calls),
            fail_idle_times: Arc::new(AtomicU32::new(0)),
        };
        let events: Arc<Mutex<Vec<SyncEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let events_for_worker = Arc::clone(&events);
        let handle = start(path.clone(), factory, move |e| {
            events_for_worker.lock().unwrap().push(e);
        });

        wait_until(
            || {
                idle_folders
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|folder| folder == "INBOX1")
            },
            Duration::from_secs(2),
        );

        let click_id = MessageId("click-acc2".into());
        let sent_at = Instant::now();
        handle.warm_bodies(
            AccountId("acc1".into()),
            vec![MessageId("warm-acc1".into())],
        );
        handle.fetch_body(AccountId("acc2".into()), click_id.clone());

        wait_until(
            || {
                events.lock().unwrap().iter().any(|e| {
                    matches!(
                        e,
                        SyncEvent::BodyReady {
                            message_id,
                            ok: true
                        } if message_id == &click_id
                    )
                })
            },
            Duration::from_secs(2),
        );
        let elapsed = sent_at.elapsed();
        handle.shutdown();

        assert!(
            elapsed < Duration::from_secs(2),
            "a click for acc2 queued behind a warm-up for acc1 must not wait out \
             MULTI_ACCOUNT_IDLE_SECS: took {elapsed:?}"
        );
        assert!(
            body_calls
                .lock()
                .unwrap()
                .iter()
                .any(|(account, id)| account == "acc2" && id == &click_id),
            "open_one_body must run on acc2's session, got: {:?}",
            body_calls.lock().unwrap()
        );
    }

    /// T-118 end-to-end: a folder is mid-backfill (the 80k-header case)
    /// when the reader clicks a letter. T-115 already made IDLE yield;
    /// the same click during `sync_one_folder` used to sit in
    /// `pending_fetches` until the last UID. BodyReady must land in well
    /// under the 30s fake backfill.
    #[test]
    fn a_click_is_not_left_behind_a_header_backfill() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        seed_account_with_due_folder(&path, "acc1", "acc1:inbox", "INBOX");

        let sync_started = Arc::new(AtomicU32::new(0));
        let body_calls = Arc::new(Mutex::new(Vec::new()));
        let factory = LongBackfillFactory {
            sync_started: Arc::clone(&sync_started),
            body_calls: Arc::clone(&body_calls),
        };
        let events: Arc<Mutex<Vec<SyncEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let events_for_worker = Arc::clone(&events);
        let handle = start(path.clone(), factory, move |e| {
            events_for_worker.lock().unwrap().push(e);
        });

        wait_until(
            || sync_started.load(Ordering::SeqCst) > 0,
            Duration::from_secs(2),
        );

        let click_id = MessageId("click-during-backfill".into());
        let sent_at = Instant::now();
        handle.fetch_body(AccountId("acc1".into()), click_id.clone());

        wait_until(
            || {
                events.lock().unwrap().iter().any(|e| {
                    matches!(
                        e,
                        SyncEvent::BodyReady {
                            message_id,
                            ok: true
                        } if message_id == &click_id
                    )
                })
            },
            Duration::from_secs(2),
        );
        let elapsed = sent_at.elapsed();
        handle.shutdown();

        assert!(
            elapsed < Duration::from_secs(2),
            "a click during a 30s header backfill must be served at the next \
             batch, not after the mailbox: took {elapsed:?}"
        );
        assert!(
            body_calls
                .lock()
                .unwrap()
                .iter()
                .any(|(account, id)| account == "acc1" && id == &click_id),
            "open_one_body must run on the live session, got: {:?}",
            body_calls.lock().unwrap()
        );
    }

    /// Fork #2: `SyncHandle::shutdown` blocks until the worker thread
    /// actually joins (unlike `Drop`, D11) -- so if `idle_once` ever
    /// stopped checking `should_stop` promptly and instead only returned
    /// once its full ceiling elapsed, this would hang for as long as that
    /// ceiling (up to 29 real minutes for a genuine `IDLE`, RFC 2177).
    /// Run `shutdown` on its own thread and bound the wait with
    /// `recv_timeout` (never a bare `recv`) so a regression fails this
    /// test by name instead of hanging the whole suite.
    #[test]
    fn shutdown_sent_during_an_idle_round_returns_quickly_instead_of_hanging() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        seed_account_with_due_folder(&path, "acc1", "inbox", "INBOX");
        // See `a_fetch_body_sent_during_an_idle_round_is_not_dropped_and_
        // does_not_wait_out_the_ceiling`'s own comment just above its
        // identical stamp: real `SystemClock`, so this needs the real
        // current time, not a fixed simulated epoch.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        stamp_folder_last_sync_at(&path, "acc1", "inbox", now);

        let idle_calls = Arc::new(AtomicU32::new(0));
        let factory = IdleBlockingFactory {
            connects: Arc::new(AtomicU32::new(0)),
            idle_calls: Arc::clone(&idle_calls),
            idle_folders: Arc::new(Mutex::new(Vec::new())),
            body_calls: Arc::new(Mutex::new(Vec::new())),
            fail_idle_times: Arc::new(AtomicU32::new(0)),
        };
        let handle = start(path.clone(), factory, |_e| {});

        wait_until(
            || idle_calls.load(Ordering::SeqCst) > 0,
            Duration::from_secs(2),
        );

        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let started = Instant::now();
            handle.shutdown();
            let _ = done_tx.send(started.elapsed());
        });

        let elapsed = done_rx.recv_timeout(Duration::from_secs(2)).expect(
            "shutdown() sent while the worker is parked in an IDLE round must return within \
             2s, not hang -- a real IDLE ceiling can be up to 29 minutes (RFC 2177)",
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "shutdown during IDLE took {elapsed:?} -- should_stop must be checked on a short \
             poll slice, not only once the full ceiling elapses"
        );
    }

    /// Fork #4: a connection that drops mid-`IDLE` (RFC 2177: an idling
    /// connection can simply go away) must reconnect through the exact
    /// same `ProviderFactory::connect` + `connect_backoff` bookkeeping an
    /// ordinary connect failure already uses -- not a second, parallel
    /// mechanism. Proved by counters and [`FakeClock`]'s simulated time,
    /// the same way `network_failures_back_off_instead_of_hot_looping`
    /// proves it for ordinary apply failures: one failing `idle_once`,
    /// then a successful reconnect exactly `2` simulated seconds later --
    /// D32's first backoff step, not sooner (a hot loop) and not later
    /// (a stuck account or a differently-tuned second mechanism).
    #[test]
    fn a_connection_lost_during_idle_reconnects_through_the_same_backoff_as_an_ordinary_connect_failure(
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        seed_account_with_due_folder(&path, "acc1", "inbox", "INBOX");
        let start_at = 1_700_000_000i64;
        stamp_folder_last_sync_at(&path, "acc1", "inbox", start_at);

        let connects = Arc::new(AtomicU32::new(0));
        let factory = IdleBlockingFactory {
            connects: Arc::clone(&connects),
            idle_calls: Arc::new(AtomicU32::new(0)),
            idle_folders: Arc::new(Mutex::new(Vec::new())),
            body_calls: Arc::new(Mutex::new(Vec::new())),
            fail_idle_times: Arc::new(AtomicU32::new(1)),
        };
        let events: Arc<Mutex<Vec<SyncEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let events_for_worker = Arc::clone(&events);
        let clock = FakeClock::new(start_at);
        let now_handle = clock.shared_now();
        let handle = start_with_clock(
            path.clone(),
            factory,
            move |e| events_for_worker.lock().unwrap().push(e),
            clock,
            FixedPower(false),
        );

        wait_until(
            || connects.load(Ordering::SeqCst) >= 2,
            Duration::from_secs(2),
        );
        handle.shutdown();

        assert_eq!(
            connects.load(Ordering::SeqCst),
            2,
            "exactly one reconnect after the single simulated IDLE connection drop"
        );
        let elapsed_simulated = *now_handle.lock().unwrap() - start_at;
        assert_eq!(
            elapsed_simulated, 2,
            "a connection lost during IDLE must back off exactly like an ordinary connect \
             failure -- D32's first step (2s), not immediately (a hot loop) and not some other \
             delay (a second, parallel backoff mechanism)"
        );
        let fired = events.lock().unwrap();
        assert!(
            fired.iter().any(|e| matches!(
                e,
                SyncEvent::ConnectFailed { account_id, .. } if account_id.as_str() == "acc1"
            )),
            "the IDLE connection drop must be reported the same way an ordinary connect \
             failure is: {fired:?}"
        );
    }

    /// Accepted on this ticket's first pass, but wrong: `idle_capable`
    /// was written by `MailSession::idle_once` and never read anywhere,
    /// so a server without `IMAP IDLE` support (D30's `!caps.idle` case,
    /// [`IdleOutcome::TimedOut`] returned immediately, not after a wait)
    /// sent `watch_inbox_for_push` straight back into another `SELECT` +
    /// `CAPABILITY` + `NOOP` round trip with nothing pacing it -- a live
    /// server hammered in a tight loop, the exact class of bug this
    /// ticket exists to not create. [`NoIdleHammerProvider`] models that
    /// server: every `idle_once` call returns `idle_capable: false` /
    /// `TimedOut` immediately, with no delay of its own, so any pacing
    /// observed can only come from `watch_inbox_for_push` honoring
    /// `NO_IDLE_POLL_SECS` through `wait_for_shutdown`'s
    /// `recv_timeout`-bounded wait -- and [`FakeClock::wait`] itself
    /// still spends a real ~20ms per call on its own `recv_timeout`
    /// (see its doc comment), which is what turns "paced" into a small,
    /// bounded call count over a real observation window instead of a
    /// simulated one this test cannot use to *count* anything.
    #[test]
    fn a_server_without_idle_paces_itself_by_no_idle_poll_secs_instead_of_hammering_the_server() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        seed_account_with_due_folder(&path, "acc1", "inbox", "INBOX");
        // `NO_IDLE_POLL_SECS` (60s) is itself bigger than
        // `INBOX_INTERVAL_SECS` (45s), so a single pacing round is enough
        // to make a freshly-due-stamped inbox look due again -- unlike
        // `a_connection_lost_during_idle_reconnects_...` above, which only
        // ever needs 2 simulated seconds of headroom, this test cannot
        // rely on `stamp_folder_last_sync_at` matching the clock's start
        // value to stay "not due" across many rounds. Without this,
        // `sync_one_due_folder` would fire on some passes -- it fails
        // against `NoIdleHammerProvider::sync_one_folder`'s stub and loops
        // straight back to the top with no wait at all, breaking any
        // relationship between `idle_calls` and simulated-clock
        // advancement. `mark_folder_already_synced` (`FAR_FUTURE_SYNC_AT`)
        // is the one seed that stays "not due" regardless of how far the
        // simulated clock advances, so every single loop pass is
        // guaranteed to reach `watch_inbox_for_push` -> `idle_once` ->
        // the pacing wait under test, and nothing else in this scenario
        // ever moves the simulated clock (connecting succeeds on the
        // first try, so no connect backoff wait fires either).
        mark_folder_already_synced(&path, "acc1", "inbox");

        let idle_calls = Arc::new(AtomicU32::new(0));
        let factory = NoIdleHammerFactory {
            idle_calls: Arc::clone(&idle_calls),
        };
        let start_at = 1_700_000_000i64;
        let clock = FakeClock::new(start_at);
        let now_handle = clock.shared_now();
        let handle = start_with_clock(path.clone(), factory, |_| {}, clock, FixedPower(false));

        // A real observation window -- this is exactly what makes the
        // regression visible: without pacing, the unmutated bug measured
        // roughly a thousand `idle_once` calls per second (see this
        // ticket's own accepted-then-rejected report); with pacing, each
        // round costs ~20ms of *real* time in `FakeClock::wait`'s own
        // `recv_timeout`, so a few hundred milliseconds of wall clock can
        // only ever produce a handful of calls.
        std::thread::sleep(Duration::from_millis(300));
        handle.shutdown();

        let calls = idle_calls.load(Ordering::SeqCst);
        assert!(
            calls <= 30,
            "a server without IMAP IDLE must be paced by NO_IDLE_POLL_SECS between polls, not \
             hammered in a tight loop -- {calls} idle_once calls in 300ms of wall time is a hot \
             loop, not a paced fallback"
        );

        // `calls <= 30` alone only proves *some* pause exists between
        // rounds -- `FakeClock::wait` spends a real ~20ms per call no
        // matter what `timeout_secs` it's asked for (see its doc
        // comment), so a wall-clock-bounded call count can't tell
        // `NO_IDLE_POLL_SECS` (60s) apart from, say, 1s: either way the
        // real 20ms floor per call caps this window to a similar handful
        // of rounds. Only the *simulated* clock says what duration each
        // round actually asked `wait_for_shutdown` to wait for. Every
        // `idle_once` call here takes the `TimedOut{idle_capable: false}`
        // arm, which makes exactly one `wait_for_shutdown` call before
        // the loop returns to `idle_once` again -- so simulated time
        // should advance by `NO_IDLE_POLL_SECS` per completed round.
        // The one exception is the very last round: `handle.shutdown()`'s
        // command can be caught by that round's own `recv_timeout(20ms)`
        // instead of it timing out, in which case that final round never
        // advances the clock at all -- hence the `- 1` lower bound
        // instead of requiring every one of `calls` rounds to have timed
        // out.
        let elapsed_simulated = *now_handle.lock().unwrap() - start_at;
        let lower = (calls.saturating_sub(1) as i64) * NO_IDLE_POLL_SECS as i64;
        let upper = calls as i64 * NO_IDLE_POLL_SECS as i64;
        assert!(
            elapsed_simulated >= lower && elapsed_simulated <= upper,
            "each TimedOut{{idle_capable: false}} round must pace itself by exactly \
             NO_IDLE_POLL_SECS ({NO_IDLE_POLL_SECS}s) of simulated time before the next \
             idle_once, not some other duration -- idle_calls={calls}, simulated \
             elapsed={elapsed_simulated}s, expected within [{lower}, {upper}]"
        );
    }

    // --- T-089 remainder: IDLE follows the open folder, and more than
    // one account still gets a push-watch turn (a short slice, then
    // round-robin) instead of falling back to `next_sync` polling.

    fn watch_input(id: &str, role: FolderRole) -> FolderInput {
        FolderInput {
            id: id.into(),
            role,
            last_synced_at: None,
            last_attempt_at: None,
            consecutive_failures: 0,
        }
    }

    #[test]
    fn idle_watch_folder_prefers_the_open_folder_when_it_belongs_to_this_account() {
        let folders = vec![
            watch_input("acc1:inbox", FolderRole::Inbox),
            watch_input("acc1:projects", FolderRole::Other),
        ];
        let picked = idle_watch_folder(&folders, Some("acc1:projects"));
        assert_eq!(
            picked.map(|f| f.id.as_str()),
            Some("acc1:projects"),
            "the mailbox the user is looking at must win over Inbox -- that is T-089's artifact"
        );
    }

    #[test]
    fn idle_watch_folder_falls_back_to_inbox_when_focus_is_missing_or_someone_elses() {
        let folders = vec![
            watch_input("acc1:inbox", FolderRole::Inbox),
            watch_input("acc1:projects", FolderRole::Other),
        ];
        for (label, open) in [
            ("nothing on screen", None),
            ("another account's folder", Some("acc2:inbox")),
            ("an overlay id that matches no row", Some("starred")),
        ] {
            let picked = idle_watch_folder(&folders, open);
            assert_eq!(
                picked.map(|f| f.id.as_str()),
                Some("acc1:inbox"),
                "{label}: IDLE must stay on this account's Inbox, not go silent and not \
                 SELECT a mailbox this session cannot see"
            );
        }
    }

    #[test]
    fn idle_watch_folder_returns_none_when_there_is_neither_focus_nor_inbox() {
        let empty: Vec<FolderInput> = vec![];
        assert!(idle_watch_folder(&empty, None).is_none());
        assert!(idle_watch_folder(&empty, Some("acc1:inbox")).is_none());

        let sent_only = vec![watch_input("acc1:sent", FolderRole::Sent)];
        assert!(
            idle_watch_folder(&sent_only, None).is_none(),
            "a Sent-only account with nothing focused has no mailbox worth parking IDLE on"
        );
        assert_eq!(
            idle_watch_folder(&sent_only, Some("acc1:sent")).map(|f| f.id.as_str()),
            Some("acc1:sent"),
            "but the same folder, once opened, is still the one to watch"
        );
    }

    /// End-to-end: [`SyncHandle::report_viewport`] naming a non-Inbox
    /// folder of this account must make `idle_once` `SELECT` that
    /// mailbox's remote name, not the account's Inbox. The first pass
    /// can still hit Inbox -- the cell starts empty, and
    /// [`SyncSessionProvider::idle_once`] returns immediately -- so the
    /// proof is that Projects appears at all, not that it is first.
    #[test]
    fn idle_follows_the_open_folder_instead_of_always_watching_inbox() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        seed_account_with_due_folder(&path, "acc1", "acc1:inbox", "INBOX");
        add_resolvable_folder(&path, "acc1", "acc1:projects", "Projects", "custom");
        let start_at = 1_700_000_000i64;
        stamp_folder_last_sync_at(&path, "acc1", "acc1:inbox", start_at);
        stamp_folder_last_sync_at(&path, "acc1", "acc1:projects", start_at);

        let calls = Arc::new(Mutex::new(Vec::new()));
        let factory = SyncSessionFactory {
            fail: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            calls: Arc::clone(&calls),
        };
        let clock = FakeClock::new(start_at);
        let handle = start_with_clock(path.clone(), factory, |_e| {}, clock, FixedPower(false));
        handle.report_viewport(Viewport {
            open_folder: Some("acc1:projects".into()),
            app_backgrounded: false,
        });

        wait_until(
            || {
                calls
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|c| c.starts_with("IDLE:Projects:"))
            },
            Duration::from_secs(2),
        );
        handle.shutdown();

        let seen = calls.lock().unwrap();
        assert!(
            seen.iter().any(|c| c.starts_with("IDLE:Projects:")),
            "IDLE must SELECT the open folder's remote name (Projects), not stay glued to \
             Inbox: {seen:?}"
        );
    }

    /// A viewport that names another account's folder -- or an overlay
    /// that matches no `FolderInput` -- must not pull this account's
    /// `IDLE` off Inbox. Single-account on purpose: the only `idle_once`
    /// this worker can make is this account's, so the remote name in the
    /// log is unambiguous.
    #[test]
    fn idle_stays_on_inbox_when_the_open_folder_is_not_this_accounts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        seed_account_with_due_folder(&path, "acc1", "acc1:inbox", "INBOX");
        add_resolvable_folder(&path, "acc1", "acc1:projects", "Projects", "custom");
        let start_at = 1_700_000_000i64;
        stamp_folder_last_sync_at(&path, "acc1", "acc1:inbox", start_at);
        stamp_folder_last_sync_at(&path, "acc1", "acc1:projects", start_at);

        let calls = Arc::new(Mutex::new(Vec::new()));
        let factory = SyncSessionFactory {
            fail: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            calls: Arc::clone(&calls),
        };
        let clock = FakeClock::new(start_at);
        let handle = start_with_clock(path.clone(), factory, |_e| {}, clock, FixedPower(false));
        handle.report_viewport(Viewport {
            open_folder: Some("acc2:inbox".into()),
            app_backgrounded: false,
        });

        wait_until(
            || calls.lock().unwrap().iter().any(|c| c.starts_with("IDLE:")),
            Duration::from_secs(2),
        );
        handle.shutdown();

        let seen = calls.lock().unwrap();
        assert!(
            seen.iter().any(|c| c.starts_with("IDLE:INBOX:")),
            "a foreign/overlay focus must fall back to this account's Inbox: {seen:?}"
        );
        assert!(
            seen.iter().all(|c| !c.starts_with("IDLE:Projects:")),
            "and must not hop onto a sibling mailbox just because one exists: {seen:?}"
        );
    }

    /// SELECT-hop: the worker is already parked in `IDLE` on Inbox;
    /// reporting a different folder of the same account must break that
    /// round (`Wake` from [`SyncHandle::report_viewport`]) and the next
    /// `idle_once` must name the new mailbox. Real timing -- the fake
    /// actually blocks -- so this is the same class of proof as
    /// `a_fetch_body_sent_during_an_idle_round_is_not_dropped_...`.
    #[test]
    fn switching_the_open_folder_select_hops_idle_onto_the_new_mailbox() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        seed_account_with_due_folder(&path, "acc1", "acc1:inbox", "INBOX");
        add_resolvable_folder(&path, "acc1", "acc1:projects", "Projects", "custom");
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        stamp_folder_last_sync_at(&path, "acc1", "acc1:inbox", now);
        stamp_folder_last_sync_at(&path, "acc1", "acc1:projects", now);

        let idle_calls = Arc::new(AtomicU32::new(0));
        let idle_folders = Arc::new(Mutex::new(Vec::new()));
        let factory = IdleBlockingFactory {
            connects: Arc::new(AtomicU32::new(0)),
            idle_calls: Arc::clone(&idle_calls),
            idle_folders: Arc::clone(&idle_folders),
            body_calls: Arc::new(Mutex::new(Vec::new())),
            fail_idle_times: Arc::new(AtomicU32::new(0)),
        };
        let handle = start(path.clone(), factory, |_e| {});

        wait_until(
            || idle_folders.lock().unwrap().iter().any(|f| f == "INBOX"),
            Duration::from_secs(2),
        );

        handle.report_viewport(Viewport {
            open_folder: Some("acc1:projects".into()),
            app_backgrounded: false,
        });

        wait_until(
            || idle_folders.lock().unwrap().iter().any(|f| f == "Projects"),
            Duration::from_secs(2),
        );
        handle.shutdown();

        let seen = idle_folders.lock().unwrap().clone();
        assert!(
            seen.iter().any(|f| f == "INBOX") && seen.iter().any(|f| f == "Projects"),
            "IDLE must hop from Inbox to the newly focused mailbox after report_viewport, \
             not stay on the first SELECT: {seen:?}"
        );
    }

    /// Two accounts, both with a resolvable Inbox that is not due: each
    /// must still get an `idle_once` turn. Before this remainder the
    /// `account_count > 1` branch dropped the session and never called
    /// `watch_inbox_for_push`, so new mail waited on `next_sync`.
    #[test]
    fn two_accounts_each_get_an_idle_turn_instead_of_falling_back_to_poll() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        seed_account_with_due_folder(&path, "acc1", "acc1:inbox", "INBOX1");
        seed_account_with_due_folder(&path, "acc2", "acc2:inbox", "INBOX2");
        let start_at = 1_700_000_000i64;
        stamp_folder_last_sync_at(&path, "acc1", "acc1:inbox", start_at);
        stamp_folder_last_sync_at(&path, "acc2", "acc2:inbox", start_at);

        let calls = Arc::new(Mutex::new(Vec::new()));
        let factory = SyncSessionFactory {
            fail: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            calls: Arc::clone(&calls),
        };
        let clock = FakeClock::new(start_at);
        let handle = start_with_clock(path.clone(), factory, |_e| {}, clock, FixedPower(false));

        wait_until(
            || {
                let seen = calls.lock().unwrap();
                seen.iter().any(|c| c.starts_with("IDLE:INBOX1:"))
                    && seen.iter().any(|c| c.starts_with("IDLE:INBOX2:"))
            },
            Duration::from_secs(2),
        );
        handle.shutdown();

        let seen = calls.lock().unwrap();
        assert!(
            seen.iter().any(|c| c.starts_with("IDLE:INBOX1:"))
                && seen.iter().any(|c| c.starts_with("IDLE:INBOX2:")),
            "each account must get an IDLE turn on its own Inbox, not be left on the poll \
             schedule: {seen:?}"
        );
    }

    /// The multi-account slice must actually be shorter than the
    /// scheduler hint. Inbox is due in 45s (`INBOX_INTERVAL_SECS`);
    /// without the cap, `idle_once` would be handed 45 and the other
    /// account would wait that full interval for a turn. With the cap
    /// both calls must name [`MULTI_ACCOUNT_IDLE_SECS`].
    /// The IDLE ceiling a `SyncSessionProvider` round was handed, read
    /// back off its `IDLE:{folder}:{secs}` log line.
    fn idle_slice_secs(call: &str) -> i64 {
        call.rsplit(':')
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| panic!("not an IDLE log line: {call}"))
    }

    #[test]
    fn a_second_account_caps_the_idle_slice_so_the_other_is_not_starved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        seed_account_with_due_folder(&path, "acc1", "acc1:inbox", "INBOX1");
        seed_account_with_due_folder(&path, "acc2", "acc2:inbox", "INBOX2");
        let start_at = 1_700_000_000i64;
        stamp_folder_last_sync_at(&path, "acc1", "acc1:inbox", start_at);
        stamp_folder_last_sync_at(&path, "acc2", "acc2:inbox", start_at);

        let calls = Arc::new(Mutex::new(Vec::new()));
        let factory = SyncSessionFactory {
            fail: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            calls: Arc::clone(&calls),
        };
        let clock = FakeClock::new(start_at);
        let handle = start_with_clock(path.clone(), factory, |_e| {}, clock, FixedPower(false));

        wait_until(
            || {
                let seen = calls.lock().unwrap();
                seen.iter().any(|c| c.starts_with("IDLE:INBOX1:"))
                    && seen.iter().any(|c| c.starts_with("IDLE:INBOX2:"))
            },
            Duration::from_secs(2),
        );
        handle.shutdown();

        let seen = calls.lock().unwrap();
        let idle: Vec<_> = seen
            .iter()
            .filter(|c| c.starts_with("IDLE:"))
            .cloned()
            .collect();
        // Never above the cap, and the first round proves the cap is what
        // bit: the Inbox hint there is the full 45s. Later rounds may ask
        // for *less* than the cap and that is correct -- simulated time
        // really passes now (T-114's unspent-ceiling wait), so by the
        // second account's turn its folder is already 15s from due, and
        // idling past that would delay the sync it is due for.
        assert!(
            !idle.is_empty()
                && idle
                    .iter()
                    .all(|c| idle_slice_secs(c) <= MULTI_ACCOUNT_IDLE_SECS),
            "no multi-account IDLE ceiling may exceed MULTI_ACCOUNT_IDLE_SECS \
             ({MULTI_ACCOUNT_IDLE_SECS}); the 45s Inbox hint would starve the other \
             account: {idle:?}"
        );
        assert_eq!(
            idle_slice_secs(&idle[0]),
            MULTI_ACCOUNT_IDLE_SECS,
            "the very first round has the whole 45s hint to cap: {idle:?}"
        );
    }

    /// T-070's "10 accounts" line, the half that needs no stand: with ten
    /// mailboxes on the round robin every one of them must get its own push
    /// turn, and every slice must still be the multi-account ceiling. Two
    /// accounts prove the branch exists; ten prove nothing in it degrades
    /// with the count -- the cursor wraps, no account is skipped, and the
    /// ceiling is not derived from how many accounts share the thread (a
    /// slice divided by the account count would drop to three seconds here
    /// and turn push into a reconnect storm).
    #[test]
    fn ten_accounts_each_get_a_push_turn_with_the_same_capped_slice() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        let start_at = 1_700_000_000i64;
        for n in 1..=10 {
            let account = format!("acc{n}");
            seed_account_with_due_folder(
                &path,
                &account,
                &format!("{account}:inbox"),
                &format!("INBOX{n}"),
            );
            stamp_folder_last_sync_at(&path, &account, &format!("{account}:inbox"), start_at);
        }

        let calls = Arc::new(Mutex::new(Vec::new()));
        let factory = SyncSessionFactory {
            fail: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            calls: Arc::clone(&calls),
        };
        let clock = FakeClock::new(start_at);
        let handle = start_with_clock(path.clone(), factory, |_e| {}, clock, FixedPower(false));

        let watched = |calls: &Arc<Mutex<Vec<String>>>| -> Vec<String> {
            let seen = calls.lock().unwrap();
            (1..=10)
                .map(|n| format!("IDLE:INBOX{n}:"))
                .filter(|prefix| seen.iter().any(|c| c.starts_with(prefix)))
                .collect()
        };
        wait_until(|| watched(&calls).len() == 10, Duration::from_secs(10));
        handle.shutdown();

        let missing: Vec<String> = (1..=10)
            .map(|n| format!("INBOX{n}"))
            .filter(|inbox| {
                !watched(&calls)
                    .iter()
                    .any(|prefix| prefix == &format!("IDLE:{inbox}:"))
            })
            .collect();
        assert!(
            missing.is_empty(),
            "every account must reach its own push watch; starved: {missing:?}"
        );

        let seen = calls.lock().unwrap();
        let idle: Vec<_> = seen
            .iter()
            .filter(|c| c.starts_with("IDLE:"))
            .cloned()
            .collect();
        // The cap does not grow with the account count -- see
        // `a_second_account_caps_the_idle_slice_so_the_other_is_not_starved`
        // for why a round may legitimately ask for less than the cap.
        assert!(
            idle.iter()
                .all(|c| idle_slice_secs(c) <= MULTI_ACCOUNT_IDLE_SECS),
            "the push slice must stay within MULTI_ACCOUNT_IDLE_SECS \
             ({MULTI_ACCOUNT_IDLE_SECS}) whatever the account count: {idle:?}"
        );
        assert_eq!(
            idle_slice_secs(&idle[0]),
            MULTI_ACCOUNT_IDLE_SECS,
            "the very first round has the whole 45s hint to cap: {idle:?}"
        );
    }

    /// A session double that lets simulated time pass the way a real IDLE
    /// round does (T-114). [`SyncSessionProvider`]'s `idle_once` returns
    /// instantly, which is right for tests about *what* the worker asks for
    /// -- but useless for a test about how often it asks: with a clock only
    /// [`FakeClock::wait`] ever advances, an instant IDLE would let a
    /// thousand rounds happen inside one simulated second. Here `idle_once`
    /// advances the shared clock by exactly the ceiling it was handed, so
    /// ten simulated quiet minutes cost ten minutes of the worker's own
    /// pacing.
    struct QuietIdleFactory {
        clock: Arc<Mutex<i64>>,
        calls: Arc<Mutex<Vec<String>>>,
        connects: Arc<AtomicU32>,
        /// `true`: `idle_once` behaves like the real one -- it *is* the
        /// wait, and the ceiling it was handed has really passed by the
        /// time it returns. `false`: it answers `TimedOut` instantly
        /// without spending anything, the misbehaving-server shape the
        /// worker has to pace around itself.
        blocking: bool,
    }

    impl ProviderFactory for QuietIdleFactory {
        fn connect(&mut self, _account: &AccountId) -> Result<Box<dyn MailSession>, ConnectError> {
            self.connects.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(QuietIdleSession {
                clock: Arc::clone(&self.clock),
                calls: Arc::clone(&self.calls),
                blocking: self.blocking,
            }))
        }
    }

    struct QuietIdleSession {
        clock: Arc<Mutex<i64>>,
        calls: Arc<Mutex<Vec<String>>>,
        blocking: bool,
    }

    impl MailProvider for QuietIdleSession {
        fn apply(&mut self, _op: &Operation) -> Result<(), ApplyError> {
            Ok(())
        }
    }

    impl MailSession for QuietIdleSession {
        fn sync_one_folder(
            &mut self,
            store: &mut CoreSyncStore<'_>,
            folder: &str,
            now: i64,
            _is_cancelled: &dyn Fn() -> bool,
        ) -> Result<SyncOutcome, SyncError> {
            self.calls.lock().unwrap().push(format!("SYNC:{folder}"));
            // A successful pass has to leave `last_synced_at = now` behind,
            // exactly as `feathermail_sync::sync_folder` does through
            // `SyncStore::save_state`. That field is the one `next_sync`
            // reads for the *normal* due-at threshold; `record_sync_attempt`
            // only moves the *backoff* one, which imposes no constraint at
            // all after a success. A double that skips this stamp leaves the
            // folder due forever and turns any pacing test into a busy loop
            // measuring the fake, not the worker.
            let mut state = store.load_state(folder)?;
            state.last_synced_at = Some(now);
            store.save_state(folder, &state)?;
            Ok(SyncOutcome {
                folder: folder.to_string(),
                ..Default::default()
            })
        }

        fn open_one_body(
            &mut self,
            _core: &mut Core,
            _id: &MessageId,
            _bodies_dir: &Path,
        ) -> Result<Vec<u8>, CoreError> {
            Err(CoreError::new(
                ErrorCode::NetworkUnavailable,
                "QuietIdleSession is only ever asked to idle and sync",
            ))
        }

        fn idle_once(
            &mut self,
            folder: &str,
            idle_timeout: Duration,
            _should_stop: &mut dyn FnMut() -> bool,
        ) -> Result<IdleRound, ConnectError> {
            self.calls.lock().unwrap().push(format!("IDLE:{folder}"));
            if self.blocking {
                *self.clock.lock().unwrap() += idle_timeout.as_secs() as i64;
            }
            Ok(IdleRound {
                idle_capable: true,
                outcome: IdleOutcome::TimedOut,
            })
        }
    }

    /// Drives a worker over a single quiet account until its *simulated*
    /// clock has passed ten minutes, and reports what that cost: how far
    /// the clock got, every call the session saw, and how many connects it
    /// took (T-114). `blocking` picks which shape of `idle_once` the
    /// session double presents -- see [`QuietIdleFactory::blocking`].
    fn quiet_ten_minutes(blocking: bool) -> (i64, Vec<String>, i64) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        seed_account_with_due_folder(&path, "acc1", "acc1:inbox", "INBOX1");
        let start_at = 1_700_000_000i64;
        stamp_folder_last_sync_at(&path, "acc1", "acc1:inbox", start_at);

        let calls = Arc::new(Mutex::new(Vec::new()));
        let connects = Arc::new(AtomicU32::new(0));
        let clock = FakeClock::new(start_at);
        let shared = clock.shared_now();
        let factory = QuietIdleFactory {
            clock: Arc::clone(&shared),
            calls: Arc::clone(&calls),
            connects: Arc::clone(&connects),
            blocking,
        };
        let handle = start_with_clock(path.clone(), factory, |_e| {}, clock, FixedPower(false));

        let ten_minutes = start_at + 600;
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline && *shared.lock().unwrap() < ten_minutes {
            std::thread::sleep(Duration::from_millis(5));
        }
        handle.shutdown();

        let elapsed = *shared.lock().unwrap() - start_at;
        let seen = calls.lock().unwrap().clone();
        (elapsed, seen, connects.load(Ordering::SeqCst) as i64)
    }

    /// T-070's "idle 10 minutes: no polling storm", settled without a stand
    /// (T-114). A quiet mailbox on a quiet network must cost a bounded
    /// number of round trips: the worker paces itself by its own schedule
    /// (the IDLE ceiling, `INBOX_INTERVAL_SECS`), never by how fast the
    /// loop can come round again. The ceiling asserted here is deliberately
    /// loose -- this test is about the difference between "a dozen or two"
    /// and "hundreds", which is what a busy loop or a mis-paced no-IDLE
    /// fallback would produce.
    #[test]
    fn ten_quiet_minutes_do_not_become_a_polling_storm() {
        let (elapsed, seen, connects) = quiet_ten_minutes(true);
        assert!(
            elapsed >= 600,
            "the simulated clock never reached ten minutes ({elapsed}s); the worker is not \
             pacing itself through its own waits"
        );
        let round_trips = seen.len() as i64;
        assert!(
            round_trips <= 60,
            "ten quiet minutes must not cost {round_trips} network round trips: {seen:?}"
        );
        assert!(
            connects <= 20,
            "ten quiet minutes must not cost {connects} reconnects"
        );
        assert!(
            seen.iter().any(|c| c.starts_with("IDLE:")),
            "a quiet mailbox must still be watched, not merely polled: {seen:?}"
        );
    }

    /// The other half of T-114: the pacing must not depend on the server
    /// being well behaved. `feathermail_providers::run_idle_with` only
    /// answers `TimedOut` after really spending the ceiling, but a server
    /// that ends the IDLE the instant it starts makes a session return
    /// `TimedOut` for free -- and the worker must still not answer that by
    /// hammering `IDLE`/`DONE` as fast as the thread can run. Without
    /// `watch_inbox_for_push`'s unspent-ceiling wait this ran up more than
    /// a hundred thousand round trips in twenty seconds.
    #[test]
    fn an_idle_that_returns_for_free_is_still_paced_by_the_worker() {
        let (elapsed, seen, _connects) = quiet_ten_minutes(false);
        assert!(
            elapsed >= 600,
            "the worker must pace out the ceiling the round did not spend; \
             the simulated clock only reached {elapsed}s"
        );
        let round_trips = seen.len() as i64;
        assert!(
            round_trips <= 60,
            "an instantly-returning IDLE must not cost {round_trips} round trips in ten \
             simulated minutes"
        );
    }

    // --- T-090: `Focus`/`PowerState` get real sources -- the shell's
    // `Viewport` report (`SyncHandle::report_viewport` -> shared cell ->
    // `run`'s per-pass snapshot) for `open_folder`/`app_backgrounded`,
    // and `PowerProbe` (`SysfsPowerProbe` over /sys/class/power_supply in
    // production) for `on_battery`.

    /// Writes one sysfs power-supply entry (`<root>/<name>/type` and
    /// `<root>/<name>/status`) under a tempdir, so
    /// [`SysfsPowerProbe::on_battery`] can be tested without the CI host
    /// needing a real battery.
    fn seed_power_supply(root: &std::path::Path, name: &str, ty: &str, status: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("type"), format!("{ty}\n")).unwrap();
        std::fs::write(dir.join("status"), format!("{status}\n")).unwrap();
    }

    #[test]
    fn sysfs_power_probe_reads_a_discharging_battery_as_on_battery() {
        let dir = tempfile::tempdir().unwrap();
        seed_power_supply(dir.path(), "BAT0", "Battery", "Discharging");
        // A mains adapter next to the discharging battery must not
        // change the answer -- real systems always list one.
        seed_power_supply(dir.path(), "ADP1", "Mains", "Online");
        let probe = SysfsPowerProbe {
            root: dir.path().to_path_buf(),
        };
        assert!(probe.on_battery());
    }

    #[test]
    fn sysfs_power_probe_reads_everything_else_as_not_on_battery() {
        let probe = SysfsPowerProbe {
            root: std::path::PathBuf::from("/nonexistent/feathermail-test-sysfs"),
        };
        assert!(
            !probe.on_battery(),
            "an unreadable/missing sysfs root must fold into `false` -- see \
             PowerProbe::on_battery's doc comment on why `cannot tell` must not \
             mute sync"
        );

        let dir = tempfile::tempdir().unwrap();
        let probe = SysfsPowerProbe {
            root: dir.path().to_path_buf(),
        };
        assert!(
            !probe.on_battery(),
            "no power supplies at all (a desktop) is `false`"
        );
        seed_power_supply(dir.path(), "ADP1", "Mains", "Online");
        assert!(!probe.on_battery(), "a mains adapter is not a battery");
        for status in ["Charging", "Full", "Not charging", "Unknown"] {
            seed_power_supply(dir.path(), "BAT0", "Battery", status);
            assert!(
                !probe.on_battery(),
                "a battery that is {status} is plugged in, not on battery"
            );
        }
        // A second battery that *is* discharging wins over the first
        // one's non-discharging status (two-battery laptops exist).
        seed_power_supply(dir.path(), "BAT1", "Battery", "Discharging");
        assert!(probe.on_battery());
    }

    /// Direct [`sync_one_due_folder`] unit test (no worker thread, no
    /// channel): the *same* folder, 30 simulated seconds after its last
    /// successful sync, must be due when the shell reports it focused
    /// (`FOCUSED_INTERVAL_SECS` = 20) and not due when nothing is
    /// focused (its `custom`-kind role's base is
    /// `NORMAL_INTERVAL_SECS` = 300). This is the T-090 acceptance check
    /// "открытая папка синхронизируется чаще закрытой" at the seam where
    /// it is deterministic.
    ///
    /// The unfocused control runs **first**, deliberately: an `Attempted`
    /// pass records the attempt (see `sync_one_due_folder_records_the_
    /// attempt_on_success_and_on_failure`), which would move both clocks
    /// the focused call below measures from.
    #[test]
    fn a_focused_folder_comes_due_at_the_focused_interval_while_unfocused_it_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        seed_account_with_due_custom_folder(&path, "acc1", "f1", "F1");
        let now = 1_700_000_000i64;
        stamp_folder_last_sync_at(&path, "acc1", "f1", now - 30);

        let core = Core::open(&path).unwrap();
        let account = AccountId("acc1".into());
        let (_tx, rx) = mpsc::channel();

        let control = sync_one_due_folder(
            &core,
            &account,
            &mut SyncSessionProvider {
                fail: false,
                calls: Arc::new(Mutex::new(Vec::new())),
            },
            &schedule_at(now),
            &rx,
            &|_e| {},
            &RefCell::new(VecDeque::new()),
        );
        assert_eq!(
            control,
            FolderSyncStep::Idle {
                hint: Some(300 - 30)
            },
            "30s after its last sync, an unfocused background-tier folder must still be \
             270s away from due"
        );

        let focused = ScheduleSnapshot {
            now,
            view: Viewport {
                open_folder: Some("f1".into()),
                ..Viewport::default()
            },
            power: PowerState::default(),
        };
        let step = sync_one_due_folder(
            &core,
            &account,
            &mut SyncSessionProvider {
                fail: false,
                calls: Arc::new(Mutex::new(Vec::new())),
            },
            &focused,
            &rx,
            &|_e| {},
            &RefCell::new(VecDeque::new()),
        );
        assert!(
            matches!(step, FolderSyncStep::Attempted { .. }),
            "the same folder, reported as on screen, is due at the 20s focused interval: {step:?}"
        );
    }

    /// The power half of T-090's acceptance check ("на батарее интервалы
    /// растут"), same deterministic seam: 350 simulated seconds after the
    /// last sync a `custom`-kind folder is due at full cadence (300s
    /// base), but not under `on_battery` (x2) or `app_backgrounded` (x2)
    /// -- both must leave it 250s out. The power-saving controls run
    /// before the full-cadence one for the same attempt-recording reason
    /// the test above documents.
    #[test]
    fn on_battery_and_a_backgrounded_window_stretch_the_interval_the_same_350s_folder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        seed_account_with_due_custom_folder(&path, "acc1", "f1", "F1");
        let now = 1_700_000_000i64;
        stamp_folder_last_sync_at(&path, "acc1", "f1", now - 350);

        let core = Core::open(&path).unwrap();
        let account = AccountId("acc1".into());
        let (_tx, rx) = mpsc::channel();
        let fresh_session = || SyncSessionProvider {
            fail: false,
            calls: Arc::new(Mutex::new(Vec::new())),
        };
        let power = |on_battery, app_backgrounded| PowerState {
            on_battery,
            screen_locked_or_idle: false,
            app_backgrounded,
            network_metered: false,
        };

        for (label, on_battery, app_backgrounded) in [
            ("on battery", true, false),
            ("window backgrounded", false, true),
        ] {
            let step = sync_one_due_folder(
                &core,
                &account,
                &mut fresh_session(),
                &ScheduleSnapshot {
                    now,
                    view: Viewport {
                        app_backgrounded,
                        ..Viewport::default()
                    },
                    power: power(on_battery, app_backgrounded),
                },
                &rx,
                &|_e| {},
                &RefCell::new(VecDeque::new()),
            );
            assert_eq!(
                step,
                FolderSyncStep::Idle {
                    hint: Some(600 - 350)
                },
                "{label}: the 300s base must double to 600s, leaving 250s at +350s"
            );
        }

        let step = sync_one_due_folder(
            &core,
            &account,
            &mut fresh_session(),
            &schedule_at(now),
            &rx,
            &|_e| {},
            &RefCell::new(VecDeque::new()),
        );
        assert!(
            matches!(step, FolderSyncStep::Attempted { .. }),
            "at full cadence the same folder is 50s past its 300s interval and due now"
        );
    }

    /// The T-090 **seam** test for focus: the pure scheduling half is
    /// pinned by `a_focused_folder_comes_due_...` above, but nothing there
    /// proves `run` actually reads the cell [`SyncHandle::report_viewport`]
    /// writes -- a worker that kept passing `Focus::default()` would leave
    /// every unit test green. So this drives the real loop over the real
    /// ([`SystemClock`]) clock: a folder stamped 30s ago is not due at its
    /// 300s background interval and parks the worker in a 270s
    /// `wait_for_shutdown`; only the report's `Wake` (not just the cell
    /// write) can make it re-evaluate *promptly*, and only the cell write
    /// (not just the wake) can make that re-evaluation see the folder as
    /// focused and due. Dropping either half turns the sync from
    /// sub-second into a 270s wait, which the 5s bound below fails red.
    ///
    /// Why the real clock and not [`FakeClock`]: `FakeClock::wait`
    /// self-resolves every park in ~20ms of real time (advancing the
    /// simulated clock), so a missing `Wake` is invisible under it --
    /// the loop would come around on its own almost immediately. The
    /// 300ms sleep before reporting exists so the worker has actually
    /// parked (opened the profile, connected, evaluated not-due) by the
    /// time the report lands; reporting earlier would test the cell but
    /// not the wake, since the very first evaluation would already see
    /// the written cell.
    #[test]
    fn a_folder_reported_focused_through_the_handle_is_synced_promptly_not_after_the_background_interval(
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        seed_account_with_due_custom_folder(&path, "acc1", "f1", "F1");
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        stamp_folder_last_sync_at(&path, "acc1", "f1", now - 30);

        let calls = Arc::new(Mutex::new(Vec::new()));
        let factory = SyncSessionFactory {
            fail: Arc::new(AtomicBool::new(false)),
            calls: Arc::clone(&calls),
        };
        let events: Arc<Mutex<Vec<SyncEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let events_for_worker = Arc::clone(&events);
        let handle = start_with_clock(
            path.clone(),
            factory,
            move |e| events_for_worker.lock().unwrap().push(e),
            SystemClock,
            FixedPower(false),
        );
        // Let the worker park on the 270s unfocused hint first -- see
        // this test's doc comment for why the ordering matters.
        std::thread::sleep(Duration::from_millis(300));
        handle.report_viewport(Viewport {
            open_folder: Some("f1".into()),
            app_backgrounded: false,
        });

        // The event names the *local* folder id (`folder_id == "f1"`),
        // unlike this fake's `calls` log, which records the resolved
        // remote mailbox name ("F1") -- see
        // `worker_syncs_a_due_folder_when_idle_and_reports_the_resolved_mailbox`.
        wait_until(
            || {
                events.lock().unwrap().iter().any(
                    |e| matches!(e, SyncEvent::FolderSynced { folder_id, .. } if folder_id == "f1"),
                )
            },
            Duration::from_secs(5),
        );
        handle.shutdown();
    }

    /// The T-090 **seam** test for power, run twice through the real loop
    /// -- once on battery, once plugged in. The folder is stamped 350s
    /// before the clock's start: due immediately at full cadence, 250s
    /// out on battery. Because [`FakeClock::wait`] advances the simulated
    /// clock by exactly the hinted wait, the battery case's sync can only
    /// be recorded at `start + 250` -- a worker that ignored the probe
    /// (passing `PowerState::default()` instead, the pre-T-090 bug) would
    /// record it at `start`, deterministically, with no real-time races
    /// involved in the assertion.
    #[test]
    fn on_battery_from_the_probe_stretches_the_interval_end_to_end() {
        for (on_battery, expected_attempt_at) in [(true, 250i64), (false, 0i64)] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("mail.db");
            {
                Core::open(&path).unwrap();
            }
            seed_account_with_due_custom_folder(&path, "acc1", "f1", "F1");
            let start = 1_700_000_000i64;
            stamp_folder_last_sync_at(&path, "acc1", "f1", start - 350);

            let calls = Arc::new(Mutex::new(Vec::new()));
            let factory = SyncSessionFactory {
                fail: Arc::new(AtomicBool::new(false)),
                calls: Arc::clone(&calls),
            };
            let events: Arc<Mutex<Vec<SyncEvent>>> = Arc::new(Mutex::new(Vec::new()));
            let events_for_worker = Arc::clone(&events);
            let handle = start_with_clock(
                path.clone(),
                factory,
                move |e| events_for_worker.lock().unwrap().push(e),
                FakeClock::new(start),
                FixedPower(on_battery),
            );
            wait_until(
                || {
                    events
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|e| matches!(e, SyncEvent::FolderSynced { folder_id, .. } if folder_id == "f1"))
                },
                Duration::from_secs(2),
            );
            handle.shutdown();

            let (last_attempt_at, _) = sync_state_attempt(&path, "acc1", "f1");
            assert_eq!(
                last_attempt_at,
                start + expected_attempt_at,
                "on_battery={on_battery}: 350s elapsed is past the 300s base only when \
                 plugged in; on battery the doubled 600s interval must push the sync to \
                 start + 250"
            );
        }
    }

    /// Same seam as `on_battery_from_the_probe_...`, for the *other* half
    /// of the [`Viewport`] report: `app_backgrounded` must reach
    /// `PowerState` from the same per-pass snapshot, not stay a field the
    /// shell writes and nothing reads. Identical arithmetic (the
    /// backgrounded multiplier is also x2), so the same `start + 250`
    /// assertion pins it.
    #[test]
    fn a_backgrounded_window_reported_through_the_handle_stretches_the_interval_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        seed_account_with_due_custom_folder(&path, "acc1", "f1", "F1");
        let start = 1_700_000_000i64;
        stamp_folder_last_sync_at(&path, "acc1", "f1", start - 350);

        let calls = Arc::new(Mutex::new(Vec::new()));
        let factory = SyncSessionFactory {
            fail: Arc::new(AtomicBool::new(false)),
            calls: Arc::clone(&calls),
        };
        let events: Arc<Mutex<Vec<SyncEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let events_for_worker = Arc::clone(&events);
        let handle = start_with_clock(
            path.clone(),
            factory,
            move |e| events_for_worker.lock().unwrap().push(e),
            FakeClock::new(start),
            FixedPower(false),
        );
        handle.report_viewport(Viewport {
            open_folder: None,
            app_backgrounded: true,
        });

        wait_until(
            || {
                events.lock().unwrap().iter().any(
                    |e| matches!(e, SyncEvent::FolderSynced { folder_id, .. } if folder_id == "f1"),
                )
            },
            Duration::from_secs(2),
        );
        handle.shutdown();

        let (last_attempt_at, _) = sync_state_attempt(&path, "acc1", "f1");
        assert_eq!(
            last_attempt_at,
            start + 250,
            "a backgrounded window must double the 300s base to 600s, so 350s elapsed is \
             not due and the sync lands at start + 250 -- recorded at start would mean \
             the report never reached PowerState"
        );
    }

    // --- T-078 (b): folder-sync-on-idle wiring itself, not just the pure
    // `next_sync`/`record_sync_attempt` helpers (already covered by
    // `feathermail-sync`'s and `feathermail-core`'s own suites).

    /// Direct unit test of [`sync_one_due_folder`] (not through [`start`]):
    /// with two folders both due right now, one call must sync exactly
    /// one of them, never both -- the identical no-starvation reasoning
    /// this module's doc comment already applies to `tick_for_account`
    /// itself. Calling the private function directly (this test lives in
    /// the same module) rather than going through the worker's timing
    /// loop is what makes this assertion exact instead of racy: the
    /// worker loop calls this function repeatedly and would eventually
    /// sync both folders over several turns, which would not catch a
    /// regression to "sync every due folder in one call".
    #[test]
    fn sync_one_due_folder_syncs_exactly_one_folder_not_every_due_folder_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        seed_account_with_due_folder(&path, "acc1", "f1", "F1");
        add_due_folder(&path, "acc1", "f2", "F2");

        let core = Core::open(&path).unwrap();
        let account = AccountId("acc1".into());
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut session = SyncSessionProvider {
            fail: false,
            calls: Arc::clone(&calls),
        };
        let (_tx, rx) = mpsc::channel();

        let step = sync_one_due_folder(
            &core,
            &account,
            &mut session,
            &schedule_at(1_700_000_000),
            &rx,
            &|_e| {},
            &RefCell::new(VecDeque::new()),
        );

        assert!(matches!(step, FolderSyncStep::Attempted { .. }));
        assert_eq!(
            calls.lock().unwrap().len(),
            1,
            "exactly one folder must be synced per call, even with two folders due at once"
        );
    }

    /// [`Core::record_sync_attempt`] must actually run after every sync
    /// pass -- on success (`consecutive_failures` reset to `0`) and on
    /// failure (`consecutive_failures` incremented) -- since that is the
    /// only thing that stops [`next_sync`] reporting the same folder due
    /// forever (see this module's T-078 (b) doc comment and
    /// [`Decision`]'s own no-starvation contract). Checked against the
    /// database directly, independently of the [`SyncEvent`] the worker
    /// also emits.
    #[test]
    fn sync_one_due_folder_records_the_attempt_on_success_and_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        seed_account_with_due_folder(&path, "acc1", "f1", "F1");

        let core = Core::open(&path).unwrap();
        let account = AccountId("acc1".into());
        let (_tx, rx) = mpsc::channel();

        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut ok_session = SyncSessionProvider {
            fail: false,
            calls: Arc::clone(&calls),
        };
        let step = sync_one_due_folder(
            &core,
            &account,
            &mut ok_session,
            &schedule_at(1_000),
            &rx,
            &|_e| {},
            &RefCell::new(VecDeque::new()),
        );
        assert!(matches!(step, FolderSyncStep::Attempted { .. }));
        let (last_attempt_at, consecutive_failures) = sync_state_attempt(&path, "acc1", "f1");
        assert_eq!(last_attempt_at, 1_000);
        assert_eq!(consecutive_failures, 0);

        let mut fail_session = SyncSessionProvider {
            fail: true,
            calls: Arc::clone(&calls),
        };
        let step = sync_one_due_folder(
            &core,
            &account,
            &mut fail_session,
            &schedule_at(2_000),
            &rx,
            &|_e| {},
            &RefCell::new(VecDeque::new()),
        );
        assert!(matches!(step, FolderSyncStep::Attempted { .. }));
        let (last_attempt_at, consecutive_failures) = sync_state_attempt(&path, "acc1", "f1");
        assert_eq!(last_attempt_at, 2_000);
        assert_eq!(
            consecutive_failures, 1,
            "a failed attempt must increment consecutive_failures"
        );
    }

    // --- T-091: a failed folder sync must be distinguishable (not just
    // ok/not-ok) and a `SyncError::Auth` failure must drop the cached
    // session instead of retrying it forever.

    /// [`sync_one_due_folder`]'s (via [`run_one_folder_sync`] and
    /// [`should_drop_session`]) `drop_session` signal must be `true` on
    /// both [`feathermail_sync::SyncError::Auth`] (the session's
    /// authorization is gone) and [`feathermail_sync::SyncError::Session`]
    /// (the wire conversation broke -- connection-level, not just this
    /// one folder's attempt), and `false` on
    /// [`feathermail_sync::SyncError::Store`] (a local-database failure
    /// that leaves the socket untouched -- reconnecting would fix nothing
    /// and would just be reconnect churn) and on a plain success. The
    /// emitted [`SyncEvent::FolderSynced`] must name the reason
    /// separately (`Auth` vs the `Other` bucket both `Session` and
    /// `Store` share for that event -- see `sync_failure_reason`'s own
    /// doc comment for why that event-level collapse is fine even though
    /// `drop_session` itself cannot use it). `now` is advanced by hand
    /// between calls (this test drives `sync_one_due_folder` directly,
    /// not through [`run`]) specifically so each call is unambiguously
    /// past the previous one's D33 backoff floor -- see
    /// `feathermail_sync::schedule::next_sync`'s own no-starvation
    /// contract for why a call at the *same* `now` right after a failure
    /// would not be due yet and would tell this test nothing.
    #[test]
    fn sync_one_due_folder_reports_drop_session_on_auth_and_session_failures_but_not_store_or_success(
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        seed_account_with_due_folder(&path, "acc1", "f1", "F1");

        let core = Core::open(&path).unwrap();
        let account = AccountId("acc1".into());
        let (_tx, rx) = mpsc::channel();
        let events: Arc<Mutex<Vec<SyncEvent>>> = Arc::new(Mutex::new(Vec::new()));

        // Pass 1: SyncError::Auth -> drop_session must be true, and the
        // event must say Auth, not just "not ok".
        let mut auth_session = AuthAwareSession {
            fail: Some(SyncError::Auth),
        };
        let events_clone = Arc::clone(&events);
        let step = sync_one_due_folder(
            &core,
            &account,
            &mut auth_session,
            &schedule_at(1_000),
            &rx,
            &move |e| events_clone.lock().unwrap().push(e),
            &RefCell::new(VecDeque::new()),
        );
        assert!(
            matches!(step, FolderSyncStep::Attempted { drop_session: true }),
            "a SyncError::Auth failure must report drop_session: true"
        );

        // Pass 2, far enough past the backoff floor to be due again:
        // SyncError::Session (a connection-level failure) -> drop_session
        // must be true, same as Auth -- this is the half of T-091 that
        // was still broken: a dead socket left the cached session in
        // place forever.
        let mut session_error_session = AuthAwareSession {
            fail: Some(SyncError::Session("simulated network blip".into())),
        };
        let events_clone = Arc::clone(&events);
        let step = sync_one_due_folder(
            &core,
            &account,
            &mut session_error_session,
            &schedule_at(100_000),
            &rx,
            &move |e| events_clone.lock().unwrap().push(e),
            &RefCell::new(VecDeque::new()),
        );
        assert!(
            matches!(step, FolderSyncStep::Attempted { drop_session: true }),
            "a SyncError::Session failure (connection-level) must report drop_session: true, \
             the same as SyncError::Auth"
        );

        // Pass 3, same idea: SyncError::Store (a local-database failure,
        // socket untouched) must NOT drop the session -- reconnecting
        // would fix nothing and would just be reconnect churn on every
        // transient local hiccup.
        let mut store_error_session = AuthAwareSession {
            fail: Some(SyncError::Store("simulated sqlite hiccup".into())),
        };
        let events_clone = Arc::clone(&events);
        let step = sync_one_due_folder(
            &core,
            &account,
            &mut store_error_session,
            &schedule_at(200_000),
            &rx,
            &move |e| events_clone.lock().unwrap().push(e),
            &RefCell::new(VecDeque::new()),
        );
        assert!(
            matches!(
                step,
                FolderSyncStep::Attempted {
                    drop_session: false
                }
            ),
            "a SyncError::Store failure must not tell the caller to drop the session -- the \
             socket is fine, only the local database failed"
        );

        // Pass 4, same idea: a plain success must not drop the session
        // either.
        let mut ok_session = AuthAwareSession { fail: None };
        let events_clone = Arc::clone(&events);
        let step = sync_one_due_folder(
            &core,
            &account,
            &mut ok_session,
            &schedule_at(300_000),
            &rx,
            &move |e| events_clone.lock().unwrap().push(e),
            &RefCell::new(VecDeque::new()),
        );
        assert!(
            matches!(
                step,
                FolderSyncStep::Attempted {
                    drop_session: false
                }
            ),
            "a successful pass must not tell the caller to drop the session"
        );

        let fired: Vec<SyncEvent> = events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| matches!(e, SyncEvent::FolderSynced { .. }))
            .cloned()
            .collect();
        assert_eq!(fired.len(), 4);
        assert!(matches!(
            fired[0],
            SyncEvent::FolderSynced {
                ok: false,
                error: Some(SyncFailureReason::Auth),
                ..
            }
        ));
        assert!(matches!(
            fired[1],
            SyncEvent::FolderSynced {
                ok: false,
                error: Some(SyncFailureReason::Other),
                ..
            }
        ));
        assert!(matches!(
            fired[2],
            SyncEvent::FolderSynced {
                ok: false,
                error: Some(SyncFailureReason::Other),
                ..
            }
        ));
        assert!(matches!(
            fired[3],
            SyncEvent::FolderSynced {
                ok: true,
                error: None,
                ..
            }
        ));
    }

    /// D14: a provider's own error text (the `String` inside
    /// [`feathermail_sync::SyncError::Session`]) must never reach a
    /// [`SyncEvent`] -- only the bare [`SyncFailureReason`] tag may. Uses
    /// a distinctive marker unlikely to appear anywhere else, and checks
    /// it against `{:?}` of every fired event (not just the `error`
    /// field) so this would also catch a future field added to
    /// `SyncEvent::FolderSynced` that forwarded the message by mistake.
    #[test]
    fn folder_synced_event_never_leaks_the_providers_error_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        seed_account_with_due_folder(&path, "acc1", "f1", "F1");

        let core = Core::open(&path).unwrap();
        let account = AccountId("acc1".into());
        let (_tx, rx) = mpsc::channel();
        let secret_marker = "super-secret-imap-response-xyz-should-never-appear";
        let mut session = AuthAwareSession {
            fail: Some(SyncError::Session(secret_marker.into())),
        };
        let events: Arc<Mutex<Vec<SyncEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let events_clone = Arc::clone(&events);
        let step = sync_one_due_folder(
            &core,
            &account,
            &mut session,
            &schedule_at(1_000),
            &rx,
            &move |e| events_clone.lock().unwrap().push(e),
            &RefCell::new(VecDeque::new()),
        );
        assert!(matches!(step, FolderSyncStep::Attempted { .. }));

        // T-078 (b): a pass now announces its start as well, so the
        // outcome is picked out by variant rather than by position.
        let fired: Vec<SyncEvent> = events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| matches!(e, SyncEvent::FolderSynced { .. }))
            .cloned()
            .collect();
        assert_eq!(fired.len(), 1);
        let debug = format!("{:?}", fired[0]);
        assert!(
            !debug.contains(secret_marker),
            "SyncEvent::FolderSynced must never carry a provider's raw error text (D14); got \
             {debug}"
        );
    }

    /// The worker-level acceptance case for T-091, through [`run`] itself
    /// (via [`start_with_clock`]/[`AuthFailFactory`]), not just the
    /// direct-call unit tests above: a sync pass that fails with
    /// [`feathermail_sync::SyncError::Auth`] must make the *next* pass
    /// against this account go through a brand new session from a fresh
    /// [`ProviderFactory::connect`] call, never the same dead one --
    /// proven by session *identity* ([`AuthFailProvider::id`]), which
    /// "did mail eventually get synced" alone cannot distinguish from
    /// "the worker kept silently retrying the same broken session". Uses
    /// [`FakeClock`] so the D33 backoff the folder starts serving after
    /// the first failure elapses in simulated time, not real seconds.
    #[test]
    fn a_sync_error_auth_failure_drops_the_session_so_the_next_pass_reconnects() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        seed_account_with_due_custom_folder(&path, "acc1", "f1", "F1");

        let next_id = Arc::new(AtomicU32::new(1));
        let auth_failures_left = Arc::new(AtomicU32::new(1)); // fails exactly once
        let session_ids_used: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let connects = Arc::new(AtomicU32::new(0));
        let factory = AuthFailFactory {
            next_id: Arc::clone(&next_id),
            auth_failures_left: Arc::clone(&auth_failures_left),
            session_ids_used: Arc::clone(&session_ids_used),
            connects: Arc::clone(&connects),
        };

        let events: Arc<Mutex<Vec<SyncEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let events_for_worker = Arc::clone(&events);
        let handle = start_with_clock(
            path.clone(),
            factory,
            move |e| {
                events_for_worker.lock().unwrap().push(e);
            },
            FakeClock::new(1_000),
            FixedPower(false),
        );

        wait_until(
            || session_ids_used.lock().unwrap().len() >= 2,
            Duration::from_secs(5),
        );
        handle.shutdown();

        let ids = session_ids_used.lock().unwrap().clone();
        assert!(
            ids.len() >= 2,
            "expected at least two sync_one_folder calls, got {ids:?}"
        );
        assert_eq!(
            ids[0], 1,
            "the first pass must use the first session connect() ever handed out"
        );
        assert_ne!(
            ids[0], ids[1],
            "the pass right after a SyncError::Auth failure must use a newly connected \
             session, not the same dead one -- session ids used: {ids:?}"
        );

        let fired = events.lock().unwrap();
        assert!(fired.iter().any(|e| matches!(
            e,
            SyncEvent::FolderSynced {
                ok: false,
                error: Some(SyncFailureReason::Auth),
                ..
            }
        )));
    }

    /// The "no infinite reauth loop" acceptance case (T-091): while every
    /// sync attempt keeps failing with `SyncError::Auth`, the worker must
    /// reconnect *exactly once per failed attempt* -- never more (a hot
    /// loop reconnecting repeatedly without ever getting back to
    /// `sync_one_folder`) and never less (silently retrying the same dead
    /// session, the bug this ticket exists to fix). Checked by comparing
    /// `connects` (how many times `ProviderFactory::connect` actually ran)
    /// against the number of distinct, strictly-increasing session ids
    /// `sync_one_folder` was called on: if either drifts from the other,
    /// something is either looping or reusing a dead session.
    #[test]
    fn repeated_sync_error_auth_failures_reconnect_exactly_once_per_failure_not_a_hot_loop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        seed_account_with_due_custom_folder(&path, "acc1", "f1", "F1");

        let next_id = Arc::new(AtomicU32::new(1));
        let auth_failures_left = Arc::new(AtomicU32::new(3));
        let session_ids_used: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let connects = Arc::new(AtomicU32::new(0));
        let factory = AuthFailFactory {
            next_id: Arc::clone(&next_id),
            auth_failures_left: Arc::clone(&auth_failures_left),
            session_ids_used: Arc::clone(&session_ids_used),
            connects: Arc::clone(&connects),
        };

        let handle = start_with_clock(
            path.clone(),
            factory,
            |_e| {},
            FakeClock::new(1_000),
            FixedPower(false),
        );

        wait_until(
            || session_ids_used.lock().unwrap().len() >= 3,
            Duration::from_secs(5),
        );
        handle.shutdown();

        let ids = session_ids_used.lock().unwrap().clone();
        assert!(
            ids.len() >= 3,
            "expected at least three sync_one_folder calls, got {ids:?}"
        );
        let first_three = &ids[..3];
        assert_eq!(
            first_three,
            &[1, 2, 3],
            "each SyncError::Auth failure must be followed by exactly one reconnect before \
             the next sync attempt -- a hot loop would reconnect many times without a matching \
             sync attempt in between, and reusing a dead session would repeat the same id; got \
             {ids:?}"
        );
        // Exactly `first_three.len()` connects would be the count the
        // instant the third failure is recorded; `+ 1` accounts for the
        // one unavoidable race in this test, not in the worker: `run`
        // keeps going on its own thread after writing the third id into
        // `session_ids_used` and, per this ticket's own drop-and-
        // reconnect design, immediately starts its next `connect()`
        // before this test's `wait_until` even wakes up -- so that
        // fourth connect can already be in flight (or done) by the time
        // `handle.shutdown()` runs. What a hot loop would actually look
        // like is unbounded growth here, not one extra connect; a wider
        // bound would stop being able to tell the two apart.
        let connects = connects.load(Ordering::SeqCst) as usize;
        assert!(
            (first_three.len()..=first_three.len() + 1).contains(&connects),
            "connect() must be called (at most one more than) once per sync attempt while \
             every attempt keeps failing with SyncError::Auth -- unbounded growth here would be \
             a hot loop, fewer would mean a dead session got reused; got {connects} connects for \
             {} sync attempts",
            first_three.len()
        );
        // Honest limitation, since the `+ 1` slack above is exactly the
        // shape a real bug could hide in: this bound cannot tell "one
        // extra connect from the documented shutdown race" apart from "a
        // genuine off-by-one that always reconnects exactly once too
        // often, every run" -- both land inside `[N, N+1]`. What this
        // test *does* reliably catch is unbounded growth (a true hot
        // loop, `connects` scaling with wall-clock time rather than with
        // `first_three.len()`) and session reuse (the `first_three ==
        // [1, 2, 3]` assertion above, which has no slack at all). A
        // steady, reproducible extra reconnect would need a different
        // signal than this test's `wait_until`/`shutdown()` race to catch
        // -- e.g. a test-only hook the worker blocks on between
        // iterations -- which does not exist today and was judged not
        // worth adding for this one bound.
    }

    /// T-091 (б)'s central acceptance case, proven through the real
    /// worker loop (not a direct `run_one_folder_sync` call, unlike
    /// `sync_one_due_folder_reports_drop_session_on_auth_and_session_failures_but_not_store_or_success`
    /// above): a `SyncError::Store` failure (the local database, socket
    /// untouched) must leave the cached session in place -- the very
    /// next pass reuses the same connected session, proven by session
    /// *identity* ([`ScriptedFailProvider::id`]) -- while a
    /// `SyncError::Session` failure right after it (a connection-level
    /// break) must drop that same session, so the pass after *that* one
    /// runs against a newly connected session, not the one that broke.
    /// This is the one test the "just fix `drop_session` back to Auth-
    /// only" mutation and the "drop on every failure including Store"
    /// mutation both land on: the first collapses the `Session` step back
    /// to reusing the dead session (breaking the `assert_ne!` below), the
    /// second drops after the `Store` step too (breaking the `assert_eq!`
    /// below). Uses [`FakeClock`] so the D33 backoff each failure starts
    /// serving elapses in simulated time, not real seconds -- same setup
    /// as `a_sync_error_auth_failure_drops_the_session_so_the_next_pass_reconnects`.
    #[test]
    fn a_sync_error_session_failure_drops_the_session_but_a_sync_error_store_failure_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        seed_account_with_due_custom_folder(&path, "acc1", "f1", "F1");

        let next_id = Arc::new(AtomicU32::new(1));
        let session_ids_used: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let connects = Arc::new(AtomicU32::new(0));
        let script = Arc::new(Mutex::new(VecDeque::from([
            SyncError::Store("simulated sqlite hiccup".into()),
            SyncError::Session("simulated socket death".into()),
        ])));
        let factory = ScriptedFailFactory {
            next_id: Arc::clone(&next_id),
            script: Arc::clone(&script),
            session_ids_used: Arc::clone(&session_ids_used),
            connects: Arc::clone(&connects),
        };

        let events: Arc<Mutex<Vec<SyncEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let events_for_worker = Arc::clone(&events);
        let handle = start_with_clock(
            path.clone(),
            factory,
            move |e| {
                events_for_worker.lock().unwrap().push(e);
            },
            FakeClock::new(1_000),
            FixedPower(false),
        );

        wait_until(
            || session_ids_used.lock().unwrap().len() >= 3,
            Duration::from_secs(5),
        );
        handle.shutdown();

        let ids = session_ids_used.lock().unwrap().clone();
        assert!(
            ids.len() >= 3,
            "expected at least three sync_one_folder calls, got {ids:?}"
        );
        assert_eq!(
            ids[0], 1,
            "the first pass must use the first session connect() ever handed out"
        );
        assert_eq!(
            ids[1], ids[0],
            "a SyncError::Store failure must NOT drop the cached session -- the socket is fine, \
             only the local database failed, so the very next pass must reuse the same session \
             id, not reconnect; got {ids:?}"
        );
        assert_ne!(
            ids[2], ids[1],
            "a SyncError::Session failure (connection-level) must drop the cached session -- \
             the very next pass must use a newly connected session, not the same dead one; got \
             {ids:?}"
        );

        let fired = events.lock().unwrap();
        assert!(fired.iter().any(|e| matches!(
            e,
            SyncEvent::FolderSynced {
                ok: false,
                error: Some(SyncFailureReason::Other),
                ..
            }
        )));
    }

    /// A [`WorkerCommand::Shutdown`] sent while a sync pass is running
    /// must be drained off `rx` right there and reported back as
    /// [`FolderSyncStep::Shutdown`] -- see [`sync_one_due_folder`]'s own
    /// doc comment for why leaving it on the channel would make [`run`]'s
    /// later `wait_for_shutdown` calls wait forever for a second
    /// `Shutdown` that is never coming.
    #[test]
    fn sync_one_due_folder_consumes_a_shutdown_sent_during_the_sync_pass_and_reports_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        seed_account_with_due_folder(&path, "acc1", "f1", "F1");

        let core = Core::open(&path).unwrap();
        let account = AccountId("acc1".into());
        let (tx, rx) = mpsc::channel();
        tx.send(WorkerCommand::Shutdown).unwrap();

        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut session = SyncSessionProvider { fail: false, calls };
        let events: Arc<Mutex<Vec<SyncEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let events_clone = Arc::clone(&events);
        let step = sync_one_due_folder(
            &core,
            &account,
            &mut session,
            &schedule_at(1_000),
            &rx,
            &move |e| {
                events_clone.lock().unwrap().push(e);
            },
            &RefCell::new(VecDeque::new()),
        );

        assert!(matches!(step, FolderSyncStep::Shutdown));
        assert!(
            rx.try_recv().is_err(),
            "the Shutdown command must have been drained off rx, not left for a later reader"
        );
        // The sync pass itself still ran to completion and is still
        // recorded/reported -- cancellation is observed cooperatively by
        // `sync_folder`, not used to abort this function early.
        let fired: Vec<SyncEvent> = events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| matches!(e, SyncEvent::FolderSynced { .. }))
            .cloned()
            .collect();
        assert_eq!(fired.len(), 1);
        assert!(matches!(fired[0], SyncEvent::FolderSynced { ok: true, .. }));
    }

    /// T-080's counterpart to the `Shutdown` case just above: a
    /// `FetchBody` sent while a folder sync pass is in progress is drained
    /// off `rx` by the exact same `is_cancelled` loop (`mpsc::Receiver`
    /// has no "peek" -- see [`sync_one_due_folder`]'s own doc comment) and
    /// must not simply be discarded the way `Shutdown` would be if this
    /// function only checked for that one variant. Proves it lands in
    /// `pending_fetches` instead, where `run`'s next pass will find it.
    #[test]
    fn sync_one_due_folder_does_not_drop_a_fetch_body_sent_during_the_sync_pass() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        seed_account_with_due_folder(&path, "acc1", "f1", "F1");

        let core = Core::open(&path).unwrap();
        let account = AccountId("acc1".into());
        let (tx, rx) = mpsc::channel();
        let other_account = AccountId("acc2".into());
        let message_id = MessageId("m1".into());
        tx.send(WorkerCommand::FetchBody {
            account_id: other_account.clone(),
            message_id: message_id.clone(),
        })
        .unwrap();

        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut session = SyncSessionProvider { fail: false, calls };
        let pending_fetches: RefCell<VecDeque<PendingFetch>> = RefCell::new(VecDeque::new());
        let step = sync_one_due_folder(
            &core,
            &account,
            &mut session,
            &schedule_at(1_000),
            &rx,
            &|_e| {},
            &pending_fetches,
        );

        assert!(matches!(step, FolderSyncStep::Attempted { .. }));
        assert!(
            rx.try_recv().is_err(),
            "the FetchBody command must have been drained off rx, not left for a later reader"
        );
        assert_eq!(
            pending_fetches.into_inner(),
            VecDeque::from([PendingFetch::Body {
                account_id: other_account,
                message_id,
            }]),
            "a FetchBody seen mid-sync-pass must reach pending_fetches, not vanish with the drain"
        );
    }

    /// T-118: a click queued while headers are still coming in must abort
    /// the pass at the next batch, not sit behind the rest of an 80k
    /// mailbox. Mutation: `is_cancelled` returning only on Shutdown
    /// restores the letter that never loads.
    #[test]
    fn a_click_yields_a_header_pass_at_the_next_batch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        seed_account_with_due_folder(&path, "acc1", "f1", "F1");

        let core = Core::open(&path).unwrap();
        let account = AccountId("acc1".into());
        let (tx, rx) = mpsc::channel();
        let message_id = MessageId("click".into());
        tx.send(WorkerCommand::FetchBody {
            account_id: account.clone(),
            message_id: message_id.clone(),
        })
        .unwrap();

        let cancelled = Arc::new(AtomicBool::new(false));
        let mut session = YieldingSyncSession {
            give_up: Duration::from_secs(2),
            cancelled: Arc::clone(&cancelled),
        };
        let pending_fetches: RefCell<VecDeque<PendingFetch>> = RefCell::new(VecDeque::new());
        let started = Instant::now();
        let step = sync_one_due_folder(
            &core,
            &account,
            &mut session,
            &schedule_at(1_000),
            &rx,
            &|_e| {},
            &pending_fetches,
        );
        let elapsed = started.elapsed();

        assert!(matches!(step, FolderSyncStep::Attempted { .. }));
        assert!(
            cancelled.load(Ordering::SeqCst),
            "the header pass must yield when a click is waiting, not run to give_up"
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "yielding at the next batch is tens of milliseconds, not the \
             rest of the mailbox: took {elapsed:?}"
        );
        assert_eq!(
            pending_fetches.into_inner(),
            VecDeque::from([PendingFetch::Body {
                account_id: account,
                message_id,
            }])
        );
    }

    /// T-118: a warm-up is not a person waiting. Yielding the first-time
    /// Inbox backfill every time the list paints would starve headers.
    #[test]
    fn a_warmup_does_not_yield_a_header_pass() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        seed_account_with_due_folder(&path, "acc1", "f1", "F1");

        let core = Core::open(&path).unwrap();
        let account = AccountId("acc1".into());
        let (tx, rx) = mpsc::channel();
        tx.send(WorkerCommand::WarmBodies {
            account_id: account.clone(),
            message_ids: vec![MessageId("warm".into())],
        })
        .unwrap();

        let cancelled = Arc::new(AtomicBool::new(false));
        let mut session = YieldingSyncSession {
            give_up: Duration::from_millis(80),
            cancelled: Arc::clone(&cancelled),
        };
        let pending_fetches: RefCell<VecDeque<PendingFetch>> = RefCell::new(VecDeque::new());
        let step = sync_one_due_folder(
            &core,
            &account,
            &mut session,
            &schedule_at(1_000),
            &rx,
            &|_e| {},
            &pending_fetches,
        );

        assert!(matches!(step, FolderSyncStep::Attempted { .. }));
        assert!(
            !cancelled.load(Ordering::SeqCst),
            "a warm-up must stay queued and let the header pass finish"
        );
        assert_eq!(
            pending_fetches.into_inner(),
            VecDeque::from([PendingFetch::Warm {
                account_id: account,
                message_ids: vec![MessageId("warm".into())],
            }])
        );
    }

    /// T-119: a click waiting after a yielded header pass must not have a
    /// freshly queued warm-up shoved in front of it. Mutation: dropping
    /// the `is_foreground` guard in `enqueue_fresh_warmup` puts the cache
    /// fill back in front of the person.
    #[test]
    fn enqueue_fresh_warmup_does_not_outrank_a_click() {
        let account = AccountId("acc1".into());
        let mut pending = VecDeque::from([PendingFetch::Body {
            account_id: account.clone(),
            message_id: MessageId("click".into()),
        }]);
        enqueue_fresh_warmup(&mut pending, &account, vec![MessageId("new".into())]);
        assert_eq!(
            pending,
            VecDeque::from([PendingFetch::Body {
                account_id: account,
                message_id: MessageId("click".into()),
            }])
        );
    }

    /// T-119: new ids go in front of a leftover warm-up, without
    /// duplicating what is already queued. Mutation: `push_back` of the
    /// whole needed window puts the just-arrived letter behind the 100
    /// that were already draining.
    #[test]
    fn enqueue_fresh_warmup_prepends_only_ids_not_already_queued() {
        let account = AccountId("acc1".into());
        let mut pending = VecDeque::from([PendingFetch::Warm {
            account_id: account.clone(),
            message_ids: vec![MessageId("old".into())],
        }]);
        enqueue_fresh_warmup(
            &mut pending,
            &account,
            vec![
                MessageId("new".into()),
                MessageId("old".into()),
                MessageId("also-new".into()),
            ],
        );
        assert_eq!(
            pending,
            VecDeque::from([
                PendingFetch::Warm {
                    account_id: account.clone(),
                    message_ids: vec![MessageId("new".into()), MessageId("also-new".into())],
                },
                PendingFetch::Warm {
                    account_id: account,
                    message_ids: vec![MessageId("old".into())],
                },
            ])
        );
    }

    /// T-119: a successful header pass must itself queue a warm-up for
    /// the newest bodies still missing, so IDLE-arrived mail does not
    /// wait for a click or for GTK to rebuild the prefetch list.
    #[test]
    fn a_successful_header_pass_queues_warmup_for_bodies_still_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        seed_account_with_due_folder(&path, "acc1", "f1", "F1");
        seed_fetchable_header(&path, "acc1", "f1", "new-mail", 2_000);

        let core = Core::open(&path).unwrap();
        let account = AccountId("acc1".into());
        let (_tx, rx) = mpsc::channel();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut session = SyncSessionProvider { fail: false, calls };
        let pending_fetches: RefCell<VecDeque<PendingFetch>> = RefCell::new(VecDeque::new());
        let step = sync_one_due_folder(
            &core,
            &account,
            &mut session,
            &schedule_at(1_000),
            &rx,
            &|_e| {},
            &pending_fetches,
        );

        assert!(matches!(step, FolderSyncStep::Attempted { .. }));
        assert_eq!(
            pending_fetches.into_inner(),
            VecDeque::from([PendingFetch::Warm {
                account_id: account,
                message_ids: vec![MessageId("new-mail".into())],
            }]),
            "headers just landed: the missing body must be queued without a click"
        );
    }

    /// T-119: a failed pass leaves pending empty -- there is nothing new
    /// to fetch, and the socket may be the one about to be dropped.
    #[test]
    fn a_failed_header_pass_does_not_queue_warmup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        seed_account_with_due_folder(&path, "acc1", "f1", "F1");
        seed_fetchable_header(&path, "acc1", "f1", "new-mail", 2_000);

        let core = Core::open(&path).unwrap();
        let account = AccountId("acc1".into());
        let (_tx, rx) = mpsc::channel();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut session = SyncSessionProvider { fail: true, calls };
        let pending_fetches: RefCell<VecDeque<PendingFetch>> = RefCell::new(VecDeque::new());
        let step = sync_one_due_folder(
            &core,
            &account,
            &mut session,
            &schedule_at(1_000),
            &rx,
            &|_e| {},
            &pending_fetches,
        );

        assert!(matches!(
            step,
            FolderSyncStep::Attempted { drop_session: true }
        ));
        assert!(
            pending_fetches.into_inner().is_empty(),
            "a failed pass must not enqueue a warm-up onto a socket about to be dropped"
        );
    }

    /// T-119 end-to-end: the worker, not the test, must fetch the body of
    /// a header that arrived through a sync pass. Mutation: skip
    /// `queue_warmup_for_folder` after `run_one_folder_sync` and this
    /// waits out the timeout -- the letter stays headers-only until a
    /// click, which is the bug.
    #[test]
    fn a_header_pass_warms_new_mail_without_a_click() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        seed_account_with_due_folder(&path, "acc1", "acc1:inbox", "INBOX");
        seed_fetchable_header(&path, "acc1", "acc1:inbox", "arrived", 2_000);

        let events: Arc<Mutex<Vec<SyncEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let events_for_worker = Arc::clone(&events);
        let handle = start(
            path.clone(),
            SyncSessionFactory {
                fail: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                calls: Arc::new(Mutex::new(Vec::new())),
            },
            move |e| {
                events_for_worker.lock().unwrap().push(e);
            },
        );

        let arrived = MessageId("arrived".into());
        wait_until(
            || {
                events.lock().unwrap().iter().any(|e| {
                    matches!(
                        e,
                        SyncEvent::BodyReady {
                            message_id,
                            ok: true
                        } if message_id == &arrived
                    )
                })
            },
            Duration::from_secs(2),
        );
        handle.shutdown();
    }

    /// End-to-end acceptance case for the success path, through [`start`]
    /// like `dispatched_operation_reaches_fake_provider_and_is_acked_without_the_test_ticking`
    /// above: an account with an empty operation queue but one folder due
    /// for its first sync gets that folder synced entirely through the
    /// background worker, and the event names the resolved remote mailbox
    /// (not the local folder id) as what was actually synced.
    #[test]
    fn worker_syncs_a_due_folder_when_idle_and_reports_the_resolved_mailbox() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        seed_account_with_due_folder(&path, "acc1", "f1", "F1");

        let calls = Arc::new(Mutex::new(Vec::new()));
        let factory = SyncSessionFactory {
            fail: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            calls: Arc::clone(&calls),
        };
        let events: Arc<Mutex<Vec<SyncEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let events_for_worker = Arc::clone(&events);
        let handle = start(path.clone(), factory, move |e| {
            events_for_worker.lock().unwrap().push(e);
        });

        wait_until(
            || {
                events
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|e| matches!(e, SyncEvent::FolderSynced { .. }))
            },
            Duration::from_secs(2),
        );
        handle.shutdown();

        let fired = events.lock().unwrap();
        assert!(fired.iter().any(
            |e| matches!(e, SyncEvent::FolderSynced { folder_id, ok: true, .. } if folder_id == "f1")
        ));
        // The same log carries this fake's `idle_once` rounds
        // (`IDLE:{folder}:{secs}`); only the bare names are
        // `sync_one_folder` calls. What matters here is *what* was
        // resolved and passed through, not how many times: every sync call
        // must be the remote mailbox name `Core::remote_folder` resolved
        // ("F1"), never the local folder id ("f1").
        let seen = calls.lock().unwrap();
        let synced: Vec<_> = seen
            .iter()
            .filter(|c| !c.starts_with("IDLE:"))
            .cloned()
            .collect();
        assert!(!synced.is_empty());
        assert!(
            synced.iter().all(|name| name == "F1"),
            "sync_one_folder must be called with the remote mailbox name Core::remote_folder \
             resolved from the local folder id, not the local id itself: {seen:?}"
        );
    }

    /// A folder sync failure must never crash the worker or cost the
    /// account its round-robin turn: with two accounts, one whose only due
    /// folder fails every sync attempt and one with a normal queued
    /// operation, both must still be served -- the failing account's
    /// `FolderSynced { ok: false }` event fires, and the other account's
    /// operation is still `Acked`, all through one running worker that
    /// never panics and shuts down cleanly afterward.
    #[test]
    fn a_failing_folder_sync_does_not_crash_the_worker_or_evict_the_account_from_its_turn() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        seed_account_with_due_folder(&path, "acc1", "f1", "F1");
        seed_account_with_own_rows(&path, "acc2");
        archive_thread(&path, "acc2", "t-acc2");

        let calls = Arc::new(Mutex::new(Vec::new()));
        let factory = SyncSessionFactory {
            fail: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            calls,
        };
        let events: Arc<Mutex<Vec<SyncEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let events_for_worker = Arc::clone(&events);
        let handle = start(path.clone(), factory, move |e| {
            events_for_worker.lock().unwrap().push(e);
        });

        wait_until(
            || {
                let fired = events.lock().unwrap();
                fired
                    .iter()
                    .any(|e| matches!(e, SyncEvent::FolderSynced { ok: false, .. }))
                    && fired.iter().any(|e| matches!(e, SyncEvent::Acked { .. }))
            },
            Duration::from_secs(2),
        );
        // Must not panic and must join promptly -- proof the failure
        // above never took the worker thread down with it.
        handle.shutdown();

        let fired = events.lock().unwrap();
        assert!(fired.iter().any(
            |e| matches!(e, SyncEvent::FolderSynced { account_id, ok: false, .. } if account_id.as_str() == "acc1")
        ));
        assert!(fired.iter().any(|e| matches!(e, SyncEvent::Acked { .. })));
    }

    /// The mandatory-constraint case: when an idle account has *both* a
    /// queue retry due soon (`known_delay`, from a failed apply) and a
    /// folder-sync hint due later (a positive `in_secs` from
    /// `next_sync`'s `Decision::Next`), the worker's combined idle wait
    /// must be the shorter of the two, not their sum -- otherwise a
    /// folder-sync hint could bury a pending queue retry exactly the way
    /// this module's own doc comment says a healthy account's retry must
    /// never be buried by another account's idle poll.
    ///
    /// [`FakeClock::wait`] never actually sleeps for more than 20ms of real
    /// time regardless of the requested `timeout_secs` (it just advances
    /// its own simulated clock), so wall-clock elapsed time cannot tell
    /// "waited the minimum" and "waited the sum" apart here -- only the
    /// simulated clock's own final value can, which is why this test reads
    /// it back through [`FakeClock::shared_now`] instead.
    #[test]
    fn combined_idle_wait_after_a_folder_hint_and_a_queue_retry_takes_the_shorter_one_not_the_sum()
    {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        seed_account_folder_thread(&path, "acc1");
        archive_t1(&path, "acc1");
        let start_at = 1_700_000_000i64;
        // INBOX_INTERVAL_SECS is 45; last synced 25s "ago" means the
        // folder becomes due in exactly 20s from `start_at`.
        stamp_folder_last_sync_at(&path, "acc1", "inbox", start_at - 25);

        // Fails the queued Archive operation's first apply once, driving
        // Core::tick_for_account to Retry { delay: 2 } (D32's first step).
        let (factory, applies, _connects) = FakeFactory::new(1);
        let events: Arc<Mutex<Vec<SyncEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let events_for_worker = Arc::clone(&events);
        let clock = FakeClock::new(start_at);
        let now_handle = clock.shared_now();
        let handle = start_with_clock(
            path.clone(),
            factory,
            move |e| events_for_worker.lock().unwrap().push(e),
            clock,
            FixedPower(false),
        );

        wait_until(
            || {
                events
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|e| matches!(e, SyncEvent::Acked { .. }))
            },
            Duration::from_secs(2),
        );
        handle.shutdown();

        assert_eq!(
            applies.load(Ordering::SeqCst),
            2,
            "one failed + one retried apply"
        );
        let elapsed_simulated = *now_handle.lock().unwrap() - start_at;
        assert_eq!(
            elapsed_simulated, 2,
            "must wait only the 2s queue retry delay, not the 20s folder-sync hint or their \
             22s sum -- the combined idle wait must be the minimum of the two"
        );
    }
    /// The folder scheduler's own hint must actually shorten the worker's
    /// idle wait. Nothing else in this module proves it: the min-vs-sum
    /// test above pits the hint against a *shorter* queue retry, so the
    /// retry wins there whether the hint is honored or dropped entirely.
    /// Here there is no queued operation at all, so the only thing
    /// standing between "new mail in 20 seconds" and "new mail in fifteen
    /// minutes" is whether `hint` reaches the wait computation.
    ///
    /// Found on review of T-078 (b) by mutation: deleting `hint` from that
    /// computation left all 17 tests green.
    ///
    /// T-089 note: before `IMAP IDLE`, a single account with no queue
    /// retry pending fell back to plain [`wait_for_shutdown`], so this
    /// test used to prove the shortened wait by reading
    /// [`FakeClock::shared_now`]'s simulated advancement. That branch now
    /// goes through [`watch_inbox_for_push`] -> [`MailSession::idle_once`]
    /// instead, which (like a real `IDLE` blocked on a live socket) never
    /// calls back into [`WorkerClock`] at all -- there is no wall-clock
    /// analogue for "the server would have pushed a change N simulated
    /// seconds from now". So this test now reads back the `idle_timeout`
    /// [`SyncSessionProvider::idle_once`] was actually called with
    /// (recorded into the same `calls` log `sync_one_folder` uses, with
    /// an `IDLE:` prefix) instead of the clock: proof `hint` reached the
    /// `IDLE` ceiling is exactly as strong a proof the wait was shortened
    /// as the old clock-reading was, for this new branch.
    #[test]
    fn a_folder_due_soon_shortens_the_idle_sleep_instead_of_waiting_out_the_full_poll() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        seed_account_with_due_folder(&path, "acc1", "inbox", "INBOX");
        let start_at = 1_700_000_000i64;
        // INBOX_INTERVAL_SECS is 45; last synced 25s "ago" puts this
        // folder exactly 20s away from due -- well under the 15-minute
        // MAX_IDLE_POLL_SECS the worker falls back to with no hint.
        stamp_folder_last_sync_at(&path, "acc1", "inbox", start_at - 25);

        let calls = Arc::new(Mutex::new(Vec::new()));
        let factory = SyncSessionFactory {
            fail: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            calls: Arc::clone(&calls),
        };
        let clock = FakeClock::new(start_at);
        let handle = start_with_clock(path.clone(), factory, |_e| {}, clock, FixedPower(false));

        wait_until(
            || calls.lock().unwrap().iter().any(|c| c.starts_with("IDLE:")),
            Duration::from_secs(2),
        );
        handle.shutdown();

        let seen = calls.lock().unwrap();
        assert!(
            seen.iter().any(|c| c == "IDLE:INBOX:20"),
            "the worker must watch the inbox with an IDLE ceiling of 20s -- what the folder \
             scheduler said this folder comes due in -- not fall back to the \
             {MAX_IDLE_POLL_SECS}s bounded poll -- otherwise new mail shows up a quarter of an \
             hour late: {seen:?}"
        );
    }

    /// A folder that is *not* due yet must be left alone: no sync pass,
    /// and the caller told how long until it is. Without this, the worker
    /// syncs on every single loop iteration and never sleeps at all --
    /// which is not merely wasteful, it is a live server hammered in a
    /// tight loop.
    ///
    /// Found on review of T-078 (b) by mutation: ignoring `in_secs` did
    /// not fail the suite, it *hung* it -- a CI timeout with no message
    /// instead of a named assertion. This test makes that failure say
    /// what broke.
    #[test]
    fn a_folder_not_due_yet_is_left_alone_and_reports_how_long_until_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        seed_account_with_due_folder(&path, "acc1", "inbox", "INBOX");
        let now = 1_700_000_000i64;
        stamp_folder_last_sync_at(&path, "acc1", "inbox", now - 25);

        let core = Core::open(&path).unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut session = SyncSessionProvider {
            fail: false,
            calls: Arc::clone(&calls),
        };
        let (_tx, rx) = mpsc::channel();

        let step = sync_one_due_folder(
            &core,
            &AccountId("acc1".into()),
            &mut session,
            &schedule_at(now),
            &rx,
            &|_e| {},
            &RefCell::new(VecDeque::new()),
        );

        assert!(
            matches!(step, FolderSyncStep::Idle { hint: Some(20) }),
            "a folder 20s from due must be reported as such, not synced early"
        );
        assert!(
            calls.lock().unwrap().is_empty(),
            "nothing may go over the wire for a folder that is not due yet, got {:?}",
            calls.lock().unwrap()
        );
    }

    #[test]
    fn a_manual_refresh_syncs_the_inbox_even_when_the_scheduler_says_not_due() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        Core::open(&path).unwrap();
        seed_account_with_due_folder(&path, "acc1", "inbox", "INBOX");
        let now = 1_700_000_000i64;
        stamp_folder_last_sync_at(&path, "acc1", "inbox", now);

        let core = Core::open(&path).unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut session = SyncSessionProvider {
            fail: false,
            calls: Arc::clone(&calls),
        };
        let (_tx, rx) = mpsc::channel();
        let step = sync_forced_folder(
            &core,
            &AccountId("acc1".into()),
            &mut session,
            &schedule_at(now),
            &rx,
            &|_e| {},
            &RefCell::new(VecDeque::new()),
        );

        assert!(matches!(step, FolderSyncStep::Attempted { .. }));
        assert_eq!(calls.lock().unwrap().as_slice(), ["INBOX"]);
    }

    /// A folder with no `remote_id` -- T-077's placeholder, and what
    /// T-084's server-refused folder looks like in the database -- has
    /// nothing resolvable to `SELECT`. The pass cannot run, but the
    /// attempt must still be recorded: `next_sync` calls a folder due
    /// whenever its last attempt is old enough, so an attempt that is
    /// never written leaves that folder due on every pass forever, and
    /// the account never settles into its idle sleep.
    ///
    /// Found on review of T-078 (b) by mutation: skipping
    /// `record_sync_attempt` on the unresolvable branch left all 17 tests
    /// green.
    #[test]
    fn an_unresolvable_folder_still_records_its_attempt_instead_of_coming_due_forever() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        seed_account_with_due_folder(&path, "acc1", "resolvable", "INBOX");
        stamp_folder_last_sync_at(&path, "acc1", "resolvable", FAR_FUTURE_SYNC_AT);
        add_unresolvable_folder(&path, "acc1", "ghost");

        let core = Core::open(&path).unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut session = SyncSessionProvider {
            fail: false,
            calls: Arc::clone(&calls),
        };
        let (_tx, rx) = mpsc::channel();
        let events: Arc<Mutex<Vec<SyncEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let events_clone = Arc::clone(&events);

        let step = sync_one_due_folder(
            &core,
            &AccountId("acc1".into()),
            &mut session,
            &schedule_at(9_000),
            &rx,
            &move |e| events_clone.lock().unwrap().push(e),
            &RefCell::new(VecDeque::new()),
        );

        assert!(matches!(step, FolderSyncStep::Attempted { .. }));
        assert!(
            calls.lock().unwrap().is_empty(),
            "there is no mailbox name to SELECT, so nothing may reach the session"
        );
        let (last_attempt_at, consecutive_failures) = sync_state_attempt(&path, "acc1", "ghost");
        assert_eq!(
            last_attempt_at, 9_000,
            "the attempt must be recorded even though it never reached the wire -- otherwise \
             this folder is due again on the very next pass, forever"
        );
        assert_eq!(consecutive_failures, 1);
        let fired = events.lock().unwrap();
        // T-078 (b): the pass announces its start even when it turns out
        // to have nothing to SELECT -- and then clears itself with the
        // failure, which is exactly what keeps the indicator honest.
        assert!(
            matches!(
                fired.as_slice(),
                [
                    SyncEvent::FolderSyncStarted { .. },
                    SyncEvent::FolderSynced { ok: false, .. }
                ]
            ),
            "the failed pass must be reported like any other, got {fired:?}"
        );
    }

    // --- T-092: `fts_pending` actually gets drained by the worker ---

    /// The headline T-092 case: a profile with newly-synced messages
    /// still sitting in `fts_pending` (exactly what `CoreSyncStore::
    /// upsert_one` leaves behind after a sync pass -- this test starts
    /// from that end state directly rather than re-driving a whole fake
    /// IMAP sync, since T-048's own tests already cover the enqueue side)
    /// ends up with an empty queue and a findable message, entirely
    /// through the background worker -- the test never calls
    /// `Core::index_pending_batch` itself.
    ///
    /// Mutation-tested (see the report): commenting out this file's
    /// `drain_one_index_batch` call site in `run` leaves `fts_pending`
    /// non-empty forever, and this test fails by name on `wait_until`'s
    /// timeout.
    #[test]
    fn after_a_sync_pass_the_index_queue_drains_and_new_mail_becomes_searchable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        seed_account_with_pending_messages(&path, "acc1", 3);
        assert_eq!(
            fts_pending_count(&path),
            3,
            "seed must leave rows pending, not already indexed"
        );

        let (factory, _applies, _connects) = FakeFactory::new(0);
        let handle = start(path.clone(), factory, |_event| {});

        wait_until(|| fts_pending_count(&path) == 0, Duration::from_secs(2));
        handle.shutdown();

        assert_eq!(fts_pending_count(&path), 0);
        assert!(
            messages_fts_matches(&path, "\"t092 subject 1\""),
            "message 1's subject must be findable via messages_fts (the same MATCH query \
             Core::search issues) once the queue that covered it has drained"
        );
        assert!(
            !messages_fts_matches(&path, "nosuchtermwaseverindexed"),
            "sanity: messages_fts_matches must be able to report false, not just true"
        );
    }

    /// Requirement 1: a backlog bigger than one [`DEFAULT_INDEX_BATCH`]
    /// must not be abandoned after the very first batch.
    /// `Core::index_pending_batch` (`crates/core/src/search.rs`) caps
    /// every single call at its `limit` argument -- `drain_one_index_batch`
    /// always calls it with exactly `DEFAULT_INDEX_BATCH` -- so fully
    /// draining a backlog bigger than that number is only possible if the
    /// worker's loop actually gets back around to `drain_one_index_batch`
    /// more than once. This is the self-`Wake` mechanism's own end-to-end
    /// proof: without it, each loop turn would call
    /// `drain_one_index_batch` at most once per idle-poll interval
    /// (`MAX_IDLE_POLL_SECS`, 15 minutes with the real `SystemClock` this
    /// test uses), and `wait_until`'s 5-second budget below would time
    /// out.
    #[test]
    fn a_backlog_bigger_than_one_batch_drains_across_several_loop_turns_not_just_the_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        let total = DEFAULT_INDEX_BATCH * 2 + 37;
        seed_account_with_a_bulk_pending_backlog(&path, "acc1", total);
        assert_eq!(fts_pending_count(&path), total as i64);

        let (factory, _applies, _connects) = FakeFactory::new(0);
        let handle = start(path.clone(), factory, |_event| {});

        wait_until(|| fts_pending_count(&path) == 0, Duration::from_secs(5));
        handle.shutdown();

        assert_eq!(
            fts_pending_count(&path),
            0,
            "a backlog spanning more than one DEFAULT_INDEX_BATCH must fully drain, not stall \
             after the first batch"
        );
    }

    /// Requirement 2: `shutdown()` sent while `fts_pending` is non-empty
    /// must return once the *current* batch finishes, not once the whole
    /// queue does. `Core::index_pending_batch`'s own SQL transaction is
    /// real, synchronous SQLite work `run`'s loop cannot see a `Shutdown`
    /// command during (checked only at the top of each loop turn) -- so
    /// this is a genuine wall-clock measurement, not a `FakeClock` one,
    /// calibrated against this module's own `t092_calibration_scratch`
    /// throwaway run (kept out of the final diff, see the report): one
    /// `DEFAULT_INDEX_BATCH`-sized real batch against a fresh on-disk
    /// profile took on the order of half a second in this environment, so
    /// a backlog of 30 batches takes on the order of fifteen seconds to
    /// drain in full. `shutdown()` returning in well under that, with rows
    /// still left in `fts_pending` afterward, is exactly requirement 2:
    /// it waited for at most a batch or two, never for all thirty.
    ///
    /// The `wait_until` below (bounded separately from the timed section)
    /// exists so this test cannot pass for the wrong reason: `start()`
    /// spawns the worker thread and returns to the caller well before that
    /// thread's first loop turn actually runs, so calling `shutdown()`
    /// immediately after `start()` races the `Shutdown` command into `rx`
    /// before the worker has drained anything at all -- a trivial "fast
    /// return with an untouched queue" that looks identical to a correct
    /// implementation and to the requirement-2 regression alike, and would
    /// pass just as fast against *both*. Waiting for `fts_pending_count` to
    /// have dropped below `total` first proves the worker is genuinely
    /// mid-backlog -- has completed at least one real batch and is not yet
    /// done -- before the timed `shutdown()` call below even starts, so a
    /// fast, non-empty result afterward actually demonstrates requirement
    /// 2 instead of merely a lucky thread-scheduling race. (Caught by
    /// hand: mutating the one-batch-per-turn call site into a `while`
    /// loop that drains to empty still passed this test in its earlier,
    /// unsynchronized form -- see the report's mutation table.)
    #[test]
    fn shutdown_sent_during_a_nonempty_index_backlog_returns_without_waiting_for_it_to_drain() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        let total = DEFAULT_INDEX_BATCH * 30;
        seed_account_with_a_bulk_pending_backlog(&path, "acc1", total);

        let (factory, _applies, _connects) = FakeFactory::new(0);
        let handle = start(path.clone(), factory, |_event| {});

        wait_until(
            || fts_pending_count(&path) < total as i64,
            Duration::from_secs(5),
        );

        let started = Instant::now();
        handle.shutdown();
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_secs(3),
            "shutdown during a large index backlog took {elapsed:?} -- must return after the \
             current batch, not wait for the whole queue (a full drain of {total} rows takes on \
             the order of 15s in this environment, see this test's own doc comment)"
        );
        assert!(
            fts_pending_count(&path) > 0,
            "the queue must not have already fully drained before shutdown returned -- otherwise \
             this test proves nothing about requirement 2 either way"
        );
    }

    /// Requirement 3: an `Err` from [`IndexBatcher::index_pending_batch`]
    /// must not crash the worker and must not spin a hot loop -- proven
    /// directly against [`drain_one_index_batch`] (not the whole threaded
    /// worker: there is no way to make a real, freshly-opened SQLite
    /// profile fail `index_pending_batch` to order without corrupting the
    /// very file the rest of this module's tests also rely on, see
    /// [`IndexBatcher`]'s own doc comment) with a fake that always fails.
    ///
    /// "No hot loop" here means: a second call placed immediately after an
    /// `Err` (simulating `run`'s very next loop turn, `now` unchanged)
    /// must not touch the batcher again -- it must see the cooldown this
    /// function itself just set and return `false` without calling
    /// `index_pending_batch` a second time. A call placed after
    /// `INDEX_ERROR_COOLDOWN_SECS` have elapsed must be allowed through
    /// again. Mutation-tested (see the report): deleting the `if now <
    /// *cooldown_until { return false; }` guard makes this test's second
    /// assertion fail by name.
    /// T-134: the repair queue drains on the loop's own turns and then
    /// stops asking. A pass that leaves rows behind says "come back now";
    /// an empty queue says "nothing to hurry back for", or the worker
    /// would spin on a queue that emptied hours ago.
    #[test]
    fn the_snippet_repair_queue_drains_and_then_goes_quiet() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        let bodies_dir = dir.path().join("bodies");
        Core::open(&path).unwrap();
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "INSERT INTO accounts (id, name, email, provider, created_at, updated_at)
                 VALUES ('acct', 'Reader', 'r@example.test', 'imap', 0, 0);
                 INSERT INTO folders (id, account_id, name, kind)
                 VALUES ('acct:inbox', 'acct', 'Inbox', 'inbox');
                 INSERT INTO threads (id, account_id, folder_id, subject, snippet, date)
                 VALUES ('t1', 'acct', 'acct:inbox', 'Sale', '', 1);
                 INSERT INTO messages (id, account_id, thread_id, folder_id, date, snippet)
                 VALUES ('m1', 'acct', 't1', 'acct:inbox', 1, '&&&& text');
                 INSERT INTO snippet_repairs (message_id) VALUES ('m1');",
            )
            .unwrap();
        }

        let core = Core::open(&path).unwrap();
        let mut cooldown_until: i64 = 0;
        // One row, one batch: handled and gone, so nothing is left to
        // hurry back for even on the very first turn.
        assert!(!drain_one_snippet_repair_batch(
            &core,
            &bodies_dir,
            1_000,
            &mut cooldown_until
        ));
        let conn = rusqlite::Connection::open(&path).unwrap();
        let left: i64 = conn
            .query_row("SELECT COUNT(*) FROM snippet_repairs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(left, 0, "a handled row leaves the queue");
        assert_eq!(cooldown_until, 0, "a healthy pass starts no cooldown");
    }

    #[test]
    fn a_failing_index_batch_backs_off_and_never_hot_loops() {
        struct FailingBatcher {
            calls: std::cell::Cell<u32>,
        }
        impl IndexBatcher for FailingBatcher {
            fn index_pending_batch(
                &self,
                _bodies_dir: &Path,
                _limit: usize,
            ) -> Result<IndexBatchResult, CoreError> {
                self.calls.set(self.calls.get() + 1);
                Err(CoreError::new(ErrorCode::Conflict, "simulated failure"))
            }
        }

        let batcher = FailingBatcher {
            calls: std::cell::Cell::new(0),
        };
        let bodies_dir = std::path::PathBuf::from("/nonexistent");
        let mut cooldown_until: i64 = 0;

        // First call: the batcher fails, `drain_one_index_batch` reports
        // "nothing to hurry back for" and must have started a cooldown.
        let hurry = drain_one_index_batch(&batcher, &bodies_dir, 1_000, &mut cooldown_until);
        assert!(!hurry);
        assert_eq!(batcher.calls.get(), 1);
        assert_eq!(cooldown_until, 1_000 + INDEX_ERROR_COOLDOWN_SECS);

        // Same simulated instant, another loop turn (`run`'s own shape:
        // this function is called once per turn unconditionally): must
        // not call the batcher again -- a hot loop would call it every
        // turn regardless of the cooldown.
        let hurry_again = drain_one_index_batch(&batcher, &bodies_dir, 1_000, &mut cooldown_until);
        assert!(!hurry_again);
        assert_eq!(
            batcher.calls.get(),
            1,
            "a loop turn still inside the cooldown window must not call index_pending_batch again"
        );

        // Still inside the cooldown one second before it clears.
        let still_cooling = drain_one_index_batch(
            &batcher,
            &bodies_dir,
            1_000 + INDEX_ERROR_COOLDOWN_SECS - 1,
            &mut cooldown_until,
        );
        assert!(!still_cooling);
        assert_eq!(batcher.calls.get(), 1);

        // Cooldown cleared: the next turn is allowed to try again (and
        // fails again, extending the cooldown from the new `now`).
        let after_cooldown = drain_one_index_batch(
            &batcher,
            &bodies_dir,
            1_000 + INDEX_ERROR_COOLDOWN_SECS,
            &mut cooldown_until,
        );
        assert!(!after_cooldown);
        assert_eq!(
            batcher.calls.get(),
            2,
            "once the cooldown has elapsed, the next turn must try index_pending_batch again"
        );
    }

    /// Requirement 3, the success half: once the batcher stops failing,
    /// [`drain_one_index_batch`] must not still be refusing to call it --
    /// `cooldown_until` is reset to `0` on `Ok`, not left however the
    /// prior failure set it.
    #[test]
    fn drain_one_index_batch_clears_the_cooldown_once_a_call_succeeds() {
        struct OnceBatcher {
            remaining: std::cell::Cell<usize>,
        }
        impl IndexBatcher for OnceBatcher {
            fn index_pending_batch(
                &self,
                _bodies_dir: &Path,
                _limit: usize,
            ) -> Result<IndexBatchResult, CoreError> {
                let n = self.remaining.get();
                self.remaining.set(0);
                Ok(IndexBatchResult {
                    indexed: n,
                    remaining: 0,
                })
            }
        }
        let batcher = OnceBatcher {
            remaining: std::cell::Cell::new(5),
        };
        let bodies_dir = std::path::PathBuf::from("/nonexistent");
        // Starts already "cooling down" from some earlier failure the
        // caller had recorded.
        let mut cooldown_until: i64 = 5_000;

        let hurry = drain_one_index_batch(&batcher, &bodies_dir, 5_000, &mut cooldown_until);
        // Still inside the stale cooldown window -- must not call yet.
        assert!(!hurry);

        let hurry = drain_one_index_batch(&batcher, &bodies_dir, 5_001, &mut cooldown_until);
        assert!(
            !hurry,
            "OnceBatcher left nothing remaining, nothing to hurry back for"
        );
        assert_eq!(
            cooldown_until, 0,
            "a successful call must clear the cooldown"
        );
    }

    /// A `ProviderFactory` whose `connect` never succeeds, so the worker
    /// always lands in `connect_backoff` and never holds a session.
    struct UnreachableFactory;

    impl ProviderFactory for UnreachableFactory {
        fn connect(&mut self, _account: &AccountId) -> Result<Box<dyn MailSession>, ConnectError> {
            Err(ConnectError::network("host is not answering"))
        }
    }

    /// Like [`FakeClock`], but records every `timeout_secs` the loop asks to
    /// wait for. A zero-length wait in the "nothing to connect to" arm is a
    /// tight loop in production (`SystemClock::wait` degrades to
    /// `recv_timeout(0)`), so the recorded values are the assertion.
    struct RecordingWaitClock {
        now: Arc<Mutex<i64>>,
        waits: Arc<Mutex<Vec<i64>>>,
    }

    impl WorkerClock for RecordingWaitClock {
        fn now(&self) -> i64 {
            *self.now.lock().unwrap()
        }

        fn wait(&self, rx: &Receiver<WorkerCommand>, timeout_secs: i64) -> Option<WorkerCommand> {
            self.waits.lock().unwrap().push(timeout_secs);
            if let Ok(cmd) = rx.recv_timeout(Duration::from_millis(1)) {
                return Some(cmd);
            }
            *self.now.lock().unwrap() += timeout_secs.max(0);
            None
        }
    }

    /// `pick_account` is the loop's only reader of the live account list, so
    /// it is also what has to drop backoff entries for accounts that are no
    /// longer in the profile -- including when the list has become empty and
    /// the early return would otherwise skip the pruning entirely.
    #[test]
    fn picking_an_account_drops_backoff_entries_for_accounts_that_are_gone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        seed_account_with_own_rows(&path, "acc1");
        let core = Core::open(&path).unwrap();

        let mut backoff: HashMap<String, (u32, i64)> = HashMap::new();
        backoff.insert("acc1".to_string(), (1, 0));
        backoff.insert("removed".to_string(), (3, 0));
        let mut cursor = 0usize;
        let mut count = 0usize;

        let picked = pick_account(&core, &mut cursor, &mut count, &mut backoff, 100);
        assert_eq!(picked.as_ref().map(|a| a.as_str()), Some("acc1"));
        assert_eq!(
            backoff.keys().collect::<Vec<_>>(),
            vec!["acc1"],
            "an entry for an account the profile no longer has must not survive"
        );

        // And once the last account goes too, the map empties instead of
        // keeping a permanently-due deadline nobody can act on.
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute("DELETE FROM accounts WHERE id = 'acc1'", [])
            .unwrap();
        assert!(pick_account(&core, &mut cursor, &mut count, &mut backoff, 100).is_none());
        assert!(backoff.is_empty());
        assert_eq!(soonest_retry(&backoff, 100), None);
    }

    /// Removing the only account (Settings -> Remove, which does *not*
    /// restart the worker) must leave the loop idling, not spinning. The
    /// removed account's `connect_backoff` entry outlives it unless
    /// something prunes it, and once its `next_attempt_at` passes,
    /// `soonest_retry` answers `0` while `pick_account` answers `None` --
    /// a zero-length wait asked for again and again, i.e. a tight loop
    /// against the profile database for the life of the process.
    #[test]
    fn removing_the_last_account_does_not_leave_the_worker_spinning() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            Core::open(&path).unwrap();
        }
        seed_account_with_own_rows(&path, "acc1");

        let now = Arc::new(Mutex::new(1_000_000i64));
        let waits: Arc<Mutex<Vec<i64>>> = Arc::new(Mutex::new(Vec::new()));
        let clock = RecordingWaitClock {
            now: Arc::clone(&now),
            waits: Arc::clone(&waits),
        };
        let events: Arc<Mutex<Vec<SyncEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let events_for_worker = Arc::clone(&events);
        let handle = start_with_clock(
            path.clone(),
            UnreachableFactory,
            move |e| {
                events_for_worker.lock().unwrap().push(e);
            },
            clock,
            FixedPower(false),
        );

        // The account is unreachable, so the worker records a D32 backoff
        // entry for it.
        wait_until(
            || {
                events
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|e| matches!(e, SyncEvent::ConnectFailed { .. }))
            },
            Duration::from_secs(5),
        );

        // The user removes it. The worker keeps running against the same
        // profile -- `Msg::RemoveAccountConfirm` never restarts it.
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.busy_timeout(Duration::from_secs(5)).unwrap();
            conn.execute("DELETE FROM accounts WHERE id = 'acc1'", [])
                .unwrap();
        }

        let before = waits.lock().unwrap().len();
        wait_until(
            || waits.lock().unwrap().len() >= before + 200,
            Duration::from_secs(10),
        );
        handle.shutdown();

        let recorded = waits.lock().unwrap().clone();
        let tail: Vec<i64> = recorded[recorded.len().saturating_sub(100)..].to_vec();
        assert!(
            tail.iter().all(|secs| *secs > 0),
            "with no account left to connect to the worker must sleep, not ask for a \
             zero-length wait over and over: {tail:?}"
        );
    }
}
