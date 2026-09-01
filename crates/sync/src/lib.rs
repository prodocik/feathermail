//! Incremental metadata sync engine (T-022, first half; D9, D30, D33).
//!
//! Pure logic: no sockets, no SQLite, no GTK. The engine talks to the world
//! only through [`MailboxSession`] (what IMAP can do) and [`SyncStore`] (what
//! the local database can do). The adapters live on the sides that own the
//! concrete types: `feathermail_providers::ImapSession` implements
//! [`MailboxSession`], `feathermail_core::CoreSyncStore` implements
//! [`SyncStore`]. This crate keeps no dependency on either, which is what
//! leaves `feathermail-core` free to depend on it and drive a sync pass (D9).
//!
//! Contract (plan.md T-022 / D30 / D33):
//! - first pass over a folder pulls existing metadata in UID batches, newest
//!   first, not all at once;
//! - later passes pull only `old_uidnext:*` plus, with CONDSTORE, flags
//!   changed `CHANGEDSINCE` the last known `HIGHESTMODSEQ`;
//! - a changed `UIDVALIDITY` invalidates the folder's local state and forces
//!   a full metadata re-sync;
//! - vanished UIDs are removed locally: every completed pass re-checks the
//!   newest [`RECONCILE_WINDOW`] UIDs against the server plus one
//!   [`UID_FETCH_BATCH`] of a rolling walk down the rest of the mailbox
//!   ([`FolderSyncState::resync_cursor`]), and a server without CONDSTORE
//!   additionally gets a whole-range flags reconciliation every
//!   [`FULL_RECONCILE_INTERVAL_SECS`] (D29/D30);
//! - progress is only ever persisted after a batch's headers are durably
//!   written, so a network error mid-run cannot lose progress or leave a
//!   gap, and a resumed run never re-downloads an already-saved batch;
//! - the engine accepts a cancellation check and exits cleanly between
//!   batches (D11).

pub mod backoff;
pub mod connection;
pub mod schedule;

use std::fmt;

/// Batch size for `UID FETCH` during backfill and delta pulls. Kept small so
/// the first folder open can paint before a big mailbox finishes backfilling.
pub const UID_FETCH_BATCH: u32 = 200;

/// How many of the newest UIDs every completed pass reconciles against the
/// server (D29 "UID vanished -> локально удалить metadata+body").
///
/// Neither half of a normal pass can notice a message that disappeared: the
/// backfill walks downwards once and never returns, `pull_new_mail` only
/// ever walks UIDs *above* the cursor, and a CONDSTORE delta reports changed
/// flags, not gone mail (this crate's session trait has no VANISHED/QRESYNC
/// channel at all). So each pass re-checks the newest window — the end of
/// the mailbox where mail is actually read, filed and deleted — at a cost of
/// exactly one extra `UID FETCH`, never a `1:*` sweep.
///
/// Everything *below* the window is reached by the rolling walk instead
/// ([`FolderSyncState::resync_cursor`], T-157): one more batch per pass, so
/// the whole mailbox is still covered, just not all at once.
pub const RECONCILE_WINDOW: u32 = UID_FETCH_BATCH;

/// How rarely a folder gets a *whole-range* reconciliation instead of just
/// the newest [`RECONCILE_WINDOW`] (D30: without CONDSTORE, "иначе —
/// периодическая полная сверка флагов"). Measured from the last completed
/// pass, and only ever taken on a server that reports no `HIGHESTMODSEQ`:
/// where CONDSTORE exists the flags delta already covers the whole mailbox
/// every pass, and re-fetching every envelope in a 200k mailbox to learn
/// what CONDSTORE just told us for free is exactly the sweep this window
/// scheme exists to avoid.
///
/// Also the cool-down between two circles of the rolling walk
/// ([`FolderSyncState::resync_completed_at`]): a walk that reached UID 1
/// starts over no sooner than this, so a folder is swept end to end at
/// roughly the cadence D30 asks for whether or not the server has
/// CONDSTORE.
pub const FULL_RECONCILE_INTERVAL_SECS: i64 = 6 * 60 * 60;

/// One message's metadata (headers only — bodies are T-024). Also used for
/// flags-only updates (CONDSTORE `CHANGEDSINCE`, or a plain flags re-fetch),
/// in which case every field but `uid`/`flags` is left at its default;
/// [`SyncStore::upsert_headers`] is expected to merge rather than clobber.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HeaderMeta {
    pub uid: u32,
    pub flags: Vec<String>,
    pub internaldate: Option<String>,
    pub size_bytes: Option<u64>,
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub cc: Option<String>,
    pub subject: Option<String>,
    pub date: Option<String>,
    /// Gmail thread id from `FETCH X-GM-THRID`. `None` on generic IMAP and
    /// on flags-only updates; the store must COALESCE, not clobber.
    pub gm_thrid: Option<String>,
}

/// A UID range for `UID FETCH`. `to: None` is the IMAP `*` open upper bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UidRange {
    pub from: u32,
    pub to: Option<u32>,
}

impl UidRange {
    pub fn bounded(from: u32, to: u32) -> Self {
        Self { from, to: Some(to) }
    }

    pub fn open(from: u32) -> Self {
        Self { from, to: None }
    }
}

/// What `SELECT` reports (D33).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MailboxSnapshot {
    pub uidvalidity: u32,
    pub uidnext: u32,
    pub exists: u32,
    /// `Some` only when the server supports CONDSTORE/QRESYNC.
    pub highest_modseq: Option<u64>,
}

/// Persisted per-folder sync cursor (D33). Mirrors `feathermail_core::SyncState`
/// but is defined locally so this crate stays independent of `core`/`db` (D9)
/// — the second half of T-022 maps between the two.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FolderSyncState {
    pub uidvalidity: Option<u32>,
    pub uidnext: Option<u32>,
    pub highest_modseq: Option<u64>,
    pub last_synced_at: Option<i64>,
    /// Resume point for an in-progress newest-first backfill: the lowest UID
    /// not yet fetched (remaining work is `1..backfill_floor`). `None` means
    /// no backfill is pending (either none needed yet, or it finished).
    /// Bookkeeping beyond the ticket's four core fields — needed so a
    /// network drop mid-backfill resumes without re-downloading what was
    /// already saved.
    pub backfill_floor: Option<u32>,
    /// The `uidnext` observed when the current backfill was *started*
    /// (captured once, at first contact with the folder, and carried
    /// unchanged across every interrupted/resumed run). Once
    /// `backfill_floor` reaches `None` (backfill fully done), this becomes
    /// the ascending "new mail" cursor (`uidnext`) — it must NOT be assumed
    /// to equal whatever the *latest* `SELECT` reports, because mail can
    /// arrive while a multi-run backfill is still in flight; that mail is
    /// only ever caught by [`pull_new_mail`], never by adopting a newer
    /// snapshot's `uidnext` directly.
    pub backfill_target: Option<u32>,
    /// T-157: resume point of the rolling reconciliation walk — the highest
    /// UID below the newest [`RECONCILE_WINDOW`] that has not been checked
    /// against the server yet. `None` means no walk is in progress: either
    /// none has ever started, or the last one reached UID 1 and stamped
    /// [`Self::resync_completed_at`].
    ///
    /// The walk exists because nothing else in a pass can notice mail that
    /// vanished *below* the newest window: the backfill walks down once and
    /// never returns, `pull_new_mail` only ever goes up, and a CONDSTORE
    /// delta reports changed flags, not gone mail. The whole-range sweep
    /// [`reconcile_known_range`] takes without CONDSTORE is not available
    /// there either — with CONDSTORE it would re-read every envelope in the
    /// mailbox to learn what the delta already gave for free. So the walk
    /// moves one [`UID_FETCH_BATCH`] per pass instead: one extra `UID
    /// FETCH`, never a `1:*`, and the whole mailbox covered eventually.
    pub resync_cursor: Option<u32>,
    /// When the last rolling walk reached UID 1. The next circle starts no
    /// sooner than [`FULL_RECONCILE_INTERVAL_SECS`] after it — the same
    /// cadence D30 gives the whole-range sweep, and a separate clock from
    /// [`Self::last_synced_at`], which moves on every successful pass and
    /// so can never say when a circle closed.
    pub resync_completed_at: Option<i64>,
}

/// Everything the sync engine needs from a live IMAP connection.
pub trait MailboxSession {
    fn select(&mut self, folder: &str) -> Result<MailboxSnapshot, SyncError>;
    fn uid_fetch_headers(
        &mut self,
        folder: &str,
        range: UidRange,
    ) -> Result<Vec<HeaderMeta>, SyncError>;
    fn uid_fetch_flags_changed_since(
        &mut self,
        folder: &str,
        range: UidRange,
        modseq: u64,
    ) -> Result<Vec<HeaderMeta>, SyncError>;
    fn list_folders(&mut self) -> Result<Vec<String>, SyncError>;
    /// Fetch one message's full body by UID (T-024). `folder` is passed for
    /// the same reason it is on `uid_fetch_headers` -- symmetry and
    /// documentation of intent -- even though a concrete IMAP
    /// implementation trusts the caller to have already `select`ed the
    /// right mailbox on this session (see [`fetch_body`], which does that
    /// for you). Implementations must use a peek-style fetch that does not
    /// mark the message `\Seen` as a side effect; this crate has no way to
    /// enforce that itself since it never talks to a real server (D9), so
    /// it is a contract on implementers, verified on the concrete side
    /// (`feathermail_providers::ImapSession`) against a fake IMAP server.
    fn fetch_body(&mut self, folder: &str, uid: u32) -> Result<Vec<u8>, SyncError>;

    /// Fetch several bodies from one already-`select`ed folder.
    ///
    /// One IMAP round trip serves the whole set (`UID FETCH 12,15,19
    /// (BODY.PEEK[])`), which is the difference between the 100-message
    /// warm-up costing a hundred round trips and costing a handful. On the
    /// owner's mailbox one body took ~400 ms wall clock and only a few of
    /// those milliseconds were the bytes -- the rest was the trip.
    ///
    /// The default implementation loops [`Self::fetch_body`], so a session
    /// that has nothing better to offer (the in-memory fakes, anything a
    /// test stands up) stays correct without implementing this at all. It
    /// is the concrete IMAP session that makes it one trip.
    ///
    /// A UID the server does not return is simply absent from the result:
    /// callers match on the UIDs they get back, never on position. Bodies
    /// come back paired with their UID for exactly that reason.
    fn fetch_bodies(
        &mut self,
        folder: &str,
        uids: &[u32],
    ) -> Result<Vec<(u32, Vec<u8>)>, SyncError> {
        let mut out = Vec::with_capacity(uids.len());
        for &uid in uids {
            out.push((uid, self.fetch_body(folder, uid)?));
        }
        Ok(out)
    }
}

/// Fetch one message's body, `select`ing `folder` first (mirrors
/// [`sync_folder`]'s own rule that every fetch is preceded by a `select`
/// for the same folder in the same pass -- a session has no notion of
/// "still on the right mailbox" of its own, see [`MailboxSession::select`]).
///
/// This is the T-024 network half of the seam: "message not cached locally
/// -> fetch it -> cache it." Only the fetch lives here -- this crate has no
/// SQLite dependency and must not gain one just to close this loop (D9), so
/// it cannot call anything like `Core::store_body` itself. The caller (in
/// practice `feathermail-core`, which already depends on this crate for
/// [`SyncStore`] the same way) is responsible for checking the cache
/// first and storing the result after; see `feathermail_core::body` for
/// that half, and `feathermail_core::Core::open_body` for where the two
/// meet.
pub fn fetch_body<M: MailboxSession>(
    session: &mut M,
    folder: &str,
    uid: u32,
) -> Result<Vec<u8>, SyncError> {
    session.select(folder)?;
    session.fetch_body(folder, uid)
}

/// [`fetch_body`] for a set of UIDs: one `select`, then one fetch for all
/// of them. See [`MailboxSession::fetch_bodies`] for why the set matters.
pub fn fetch_bodies<M: MailboxSession>(
    session: &mut M,
    folder: &str,
    uids: &[u32],
) -> Result<Vec<(u32, Vec<u8>)>, SyncError> {
    if uids.is_empty() {
        return Ok(Vec::new());
    }
    session.select(folder)?;
    session.fetch_bodies(folder, uids)
}

/// Everything the sync engine needs from the local database.
pub trait SyncStore {
    fn load_state(&mut self, folder: &str) -> Result<FolderSyncState, SyncError>;
    fn save_state(&mut self, folder: &str, state: &FolderSyncState) -> Result<(), SyncError>;
    /// Upsert by UID: update the row if present (merging flags-only entries),
    /// insert otherwise.
    fn upsert_headers(&mut self, folder: &str, headers: &[HeaderMeta]) -> Result<(), SyncError>;
    /// Drop metadata for UIDs that are gone from the server (VANISHED, or
    /// simply absent from a requested range).
    fn remove_vanished(&mut self, folder: &str, uids: &[u32]) -> Result<(), SyncError>;
    /// UIDVALIDITY changed: every previously cached UID in this folder is
    /// meaningless (a UID may now name a different message), so wipe it
    /// before the forced full re-sync repopulates it.
    fn reset_folder(&mut self, folder: &str) -> Result<(), SyncError>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncError {
    Session(String),
    Store(String),
    /// The session's authorization is no longer good (T-091) -- symmetric
    /// to `feathermail_core::provider::ApplyError::Auth`, both in shape (a
    /// bare tag, no message) and in why it carries none: whatever text an
    /// IMAP server sent back is not something this crate -- or anything
    /// downstream of it, per D14 -- is willing to repeat. Only ever meant
    /// to be constructed by an adapter that told a genuine protocol-level
    /// distinction apart from a plain text guess (see
    /// `feathermail_providers::sync_session::map_err`'s own doc comment
    /// for where that distinction actually happens today, and where it
    /// currently cannot).
    Auth,
}

impl fmt::Display for SyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Session(m) => write!(f, "IMAP session error: {m}"),
            Self::Store(m) => write!(f, "sync store error: {m}"),
            Self::Auth => write!(f, "IMAP session error: authorization required"),
        }
    }
}

impl std::error::Error for SyncError {}

/// Result of one [`sync_folder`] call.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncOutcome {
    pub folder: String,
    pub headers_fetched: usize,
    pub flags_updated: usize,
    pub vanished_removed: usize,
    pub uidvalidity_reset: bool,
    pub cancelled: bool,
}

impl SyncOutcome {
    fn new(folder: &str) -> Self {
        Self {
            folder: folder.to_string(),
            ..Self::default()
        }
    }
}

/// Result of [`resync_flags_range`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FlagsResyncOutcome {
    pub checked: usize,
    pub updated: usize,
    pub vanished_removed: usize,
}

/// Run one incremental-sync pass over `folder`. `now` is the caller's clock
/// (kept out of this crate so the engine stays deterministic and testable).
/// `is_cancelled` is polled between batches (D11); on a cancellation the
/// function returns `Ok` with `cancelled: true` and whatever progress was
/// durably saved, not an error.
pub fn sync_folder<M, S>(
    session: &mut M,
    store: &mut S,
    folder: &str,
    now: i64,
    is_cancelled: &dyn Fn() -> bool,
) -> Result<SyncOutcome, SyncError>
where
    M: MailboxSession,
    S: SyncStore,
{
    let snapshot = session.select(folder)?;
    let mut state = store.load_state(folder)?;
    let mut outcome = SyncOutcome::new(folder);

    let uidvalidity_changed = matches!(state.uidvalidity, Some(old) if old != snapshot.uidvalidity);
    if uidvalidity_changed {
        outcome.uidvalidity_reset = true;
        store.reset_folder(folder)?;
        state = FolderSyncState::default();
    }

    let is_first_run = state.uidvalidity.is_none();
    if is_first_run {
        state.uidvalidity = Some(snapshot.uidvalidity);
        // Capture the modseq baseline at first contact, once, regardless of
        // whether the backfill below finishes in this same call. From this
        // point on `highest_modseq` only ever advances after a CONDSTORE
        // delta for that exact window has actually been fetched and applied
        // (see the CHANGEDSINCE step below) — never by silently adopting a
        // later `SELECT`'s (newer) value.
        state.highest_modseq = snapshot.highest_modseq;
        if snapshot.uidnext > 1 {
            state.backfill_target = Some(snapshot.uidnext);
            state.backfill_floor = Some(snapshot.uidnext);
        } else {
            // Empty mailbox: trivially already caught up, nothing to backfill.
            state.uidnext = Some(snapshot.uidnext);
            state.backfill_target = None;
            state.backfill_floor = None;
        }
        // Persist the markers before the batch loop starts: even a crash
        // right after this line leaves a resumable, well-defined state.
        store.save_state(folder, &state)?;
    }

    // A floor of 1 means "nothing left below uid 1", i.e. exactly the same
    // thing as `None`. Both spellings can reach the database: the batch loop
    // saves `Some(1)` after its last batch, and a failure later in the same
    // run (before the end-of-run save) leaves that spelling persisted. Treat
    // the two as one state here, or such a folder is read as "backfill still
    // pending" forever and stops pulling new mail entirely.
    if state.backfill_floor.is_some_and(|f| f <= 1) {
        state.backfill_floor = None;
    }

    let backfill_ran = state.backfill_floor.is_some();
    if backfill_ran {
        run_backfill(
            session,
            store,
            folder,
            &mut state,
            is_cancelled,
            &mut outcome,
        )?;
    }

    // New mail pull: independent of whether a backfill was pending, ran, or
    // just finished above — as long as no backfill remains outstanding,
    // catch the cursor up to whatever the current SELECT reports.
    if !outcome.cancelled && state.backfill_floor.is_none() {
        // The backfill is done (possibly after several interrupted runs):
        // everything below the UID it *started* at is now synced, so that
        // value — not whatever the current SELECT reports — becomes the
        // ascending "new mail" cursor. Mail that arrived while the backfill
        // was in flight is picked up by `pull_new_mail` right below, in this
        // same run. Promotion lives out here rather than inside the backfill
        // branch so that a run which only has to promote (the batch loop
        // finished in an earlier run, the pull did not) still does it.
        if let Some(target) = state.backfill_target.take() {
            state.uidnext = Some(state.uidnext.map_or(target, |u| u.max(target)));
        }
        let start = state.uidnext.unwrap_or(1);
        if start < snapshot.uidnext {
            pull_new_mail(
                session,
                store,
                folder,
                start,
                snapshot.uidnext,
                &mut state,
                is_cancelled,
                &mut outcome,
            )?;
        }
    }

    // CONDSTORE flags delta: independent of backfill/new-mail progress above
    // (a flag change on already-synced mail is orthogonal to whether older
    // mail is still being backfilled). Only ever advances `highest_modseq`
    // once the delta for that exact window has been fetched and applied —
    // never unconditionally, and never on cancellation.
    if !outcome.cancelled {
        if let (Some(old_modseq), Some(new_modseq)) =
            (state.highest_modseq, snapshot.highest_modseq)
        {
            if new_modseq > old_modseq {
                sync_condstore_flags(
                    session,
                    store,
                    folder,
                    old_modseq,
                    snapshot.uidnext,
                    is_cancelled,
                    &mut outcome,
                )?;
                if !outcome.cancelled {
                    state.highest_modseq = Some(new_modseq);
                }
            }
        }
    }

    // Reconciliation against the server (D29 vanished mail, D30 periodic
    // full flags check). Deliberately the last step of the pass: by the time
    // it runs, everything this pass owed the folder has already been fetched
    // and saved, so interrupting it costs nothing but the reconciliation
    // itself.
    //
    // Skipped while a backfill is outstanding *or* ran in this very pass:
    // `detect_vanished` over a range the store was never given rows for is
    // precisely the mistake [`fetch_range`] warns about, and a range this
    // pass just fetched has nothing to reconcile against anyway.
    if !outcome.cancelled && !backfill_ran && state.backfill_floor.is_none() {
        reconcile_known_range(
            session,
            store,
            folder,
            snapshot,
            now,
            &mut state,
            is_cancelled,
            &mut outcome,
        )?;
    }

    // A cancelled pass kept its progress but did **not** finish syncing the
    // folder, and `last_synced_at` is exactly `schedule::next_sync`'s "how
    // long since the last *successful* sync" input. Stamping it here would
    // park a half-done backfill for a whole scheduler interval every time a
    // click in the reading pane yielded the socket (T-118). `save_state`
    // stays unconditional -- it is also what persists `backfill_floor`,
    // `uidnext` and `highest_modseq`.
    if !outcome.cancelled {
        state.last_synced_at = Some(now);
    }
    store.save_state(folder, &state)?;

    Ok(outcome)
}

/// Re-check a range the store already has rows for: whatever the server
/// still returns updates flags, whatever it no longer returns is gone (D29).
///
/// Three parts, in this order:
/// 1. one [`RECONCILE_WINDOW`]-wide window at the top of the UID space,
///    every pass — the end of the mailbox where mail is actually read,
///    filed and deleted;
/// 2. on a server without CONDSTORE, and no more often than
///    [`FULL_RECONCILE_INTERVAL_SECS`], the whole synced range instead of
///    that window (D30) — which also closes the walk below, since it has
///    just checked everything the walk would;
/// 3. otherwise one [`UID_FETCH_BATCH`] of the rolling walk below the
///    window ([`FolderSyncState::resync_cursor`], T-157). That is the only
///    thing that ever looks below the window on a CONDSTORE server, where
///    part 2 is deliberately never taken.
///
/// Budget: one `UID FETCH` for the window plus at most one for the walk.
/// Never a `1:*`, and never a second walk batch to "catch up" — a pass that
/// falls behind simply covers the mailbox a little later.
///
/// Cancellation stops the walk but does **not** mark the pass cancelled:
/// the substantive sync already completed above, and reporting the pass as
/// unfinished would keep the folder permanently due — on a client that
/// yields the socket often, a sweep that can never finish would then be
/// restarted from the bottom on every single pass. For the same reason a
/// cancellation leaves `resync_cursor` exactly where it was: the batch it
/// stopped before is simply the next pass's batch.
#[allow(clippy::too_many_arguments)]
fn reconcile_known_range<M, S>(
    session: &mut M,
    store: &mut S,
    folder: &str,
    snapshot: MailboxSnapshot,
    now: i64,
    state: &mut FolderSyncState,
    is_cancelled: &dyn Fn() -> bool,
    outcome: &mut SyncOutcome,
) -> Result<(), SyncError>
where
    M: MailboxSession,
    S: SyncStore,
{
    let top = snapshot.uidnext.saturating_sub(1);
    if top == 0 {
        return Ok(());
    }
    let full_due = snapshot.highest_modseq.is_none()
        && state
            .last_synced_at
            .is_some_and(|last| now.saturating_sub(last) >= FULL_RECONCILE_INTERVAL_SECS);
    let window_bottom = top.saturating_sub(RECONCILE_WINDOW - 1).max(1);
    let bottom = if full_due { 1 } else { window_bottom };
    if !reconcile_span(session, store, folder, bottom, top, is_cancelled, outcome)? {
        return Ok(());
    }
    if full_due {
        // The sweep just read every UID the rolling walk exists to reach,
        // so the circle is closed by it rather than duplicated after it.
        state.resync_cursor = None;
        state.resync_completed_at = Some(now);
        return Ok(());
    }
    let cursor = match state.resync_cursor {
        // A mailbox that shrank (UIDs expunged from the top) can leave a
        // stored cursor above the current top; clamp rather than re-read a
        // range that no longer exists.
        Some(cursor) => cursor.min(top),
        None => {
            let due = state
                .resync_completed_at
                .is_none_or(|done| now.saturating_sub(done) >= FULL_RECONCILE_INTERVAL_SECS);
            if !due {
                return Ok(());
            }
            if window_bottom <= 1 {
                // The window already covers the whole mailbox: there is
                // nothing below it to walk, and the circle is complete the
                // moment it opens. Stamping it keeps a small folder from
                // re-deciding this on every single pass.
                state.resync_completed_at = Some(now);
                return Ok(());
            }
            window_bottom - 1
        }
    };
    if is_cancelled() {
        return Ok(());
    }
    let batch_bottom = cursor.saturating_sub(UID_FETCH_BATCH - 1).max(1);
    let checked = resync_flags_range(
        session,
        store,
        folder,
        UidRange::bounded(batch_bottom, cursor),
    )?;
    outcome.flags_updated += checked.updated;
    outcome.vanished_removed += checked.vanished_removed;
    if batch_bottom <= 1 {
        state.resync_cursor = None;
        state.resync_completed_at = Some(now);
    } else {
        state.resync_cursor = Some(batch_bottom - 1);
    }
    Ok(())
}

/// Walk `bottom..=top` in [`UID_FETCH_BATCH`]-sized chunks, re-checking
/// each against the server. `false` means a cancellation cut the walk
/// short (the caller must not then claim the span was covered).
fn reconcile_span<M, S>(
    session: &mut M,
    store: &mut S,
    folder: &str,
    bottom: u32,
    top: u32,
    is_cancelled: &dyn Fn() -> bool,
    outcome: &mut SyncOutcome,
) -> Result<bool, SyncError>
where
    M: MailboxSession,
    S: SyncStore,
{
    let mut bottom = bottom;
    loop {
        if is_cancelled() {
            return Ok(false);
        }
        let chunk_top = bottom.saturating_add(UID_FETCH_BATCH - 1).min(top);
        let checked =
            resync_flags_range(session, store, folder, UidRange::bounded(bottom, chunk_top))?;
        // Counted as a flags update, not as `headers_fetched`: nothing new
        // was discovered here, the same headers were simply re-read.
        outcome.flags_updated += checked.updated;
        outcome.vanished_removed += checked.vanished_removed;
        if chunk_top >= top {
            return Ok(true);
        }
        bottom = chunk_top + 1;
    }
}

/// Periodic full flags reconciliation for servers without CONDSTORE (D30).
/// The caller decides which range to check; this also catches vanished mail
/// in that range (a requested UID that the server no longer returns).
///
/// [`sync_folder`] drives this itself at the end of every completed pass
/// (see [`reconcile_known_range`] for the cadence and window it picks); it
/// stays public because a caller with better knowledge — a range the user is
/// actually looking at, a folder a conflict was just detected in — can ask
/// for a tighter range directly.
pub fn resync_flags_range<M, S>(
    session: &mut M,
    store: &mut S,
    folder: &str,
    range: UidRange,
) -> Result<FlagsResyncOutcome, SyncError>
where
    M: MailboxSession,
    S: SyncStore,
{
    let to = range
        .to
        .ok_or_else(|| SyncError::Session("resync_flags_range needs a bounded range".into()))?;
    let checked = (to.saturating_sub(range.from) as usize) + 1;
    // Reconciliation re-checks a range we've already synced before, so an
    // absent UID here genuinely means the message is gone.
    let stats = fetch_range(session, store, folder, range, true)?;
    Ok(FlagsResyncOutcome {
        checked,
        updated: stats.fetched,
        vanished_removed: stats.vanished,
    })
}

struct FetchStats {
    fetched: usize,
    vanished: usize,
}

/// Fetch one bounded UID range and upsert what came back. When
/// `detect_vanished` is set, any requested UID absent from the response is
/// treated as vanished and removed locally — correct only when the range
/// was previously known to the store (a reconciliation pass). During a
/// first-time backfill or an ascending pull of brand-new mail, the local
/// store never had rows for those UIDs to begin with, so `detect_vanished`
/// must be `false`: on a large mailbox, treating an untouched range as
/// "vanished" turns the very first sync into tens of thousands of pointless
/// delete calls against rows that were never written.
fn fetch_range<M, S>(
    session: &mut M,
    store: &mut S,
    folder: &str,
    range: UidRange,
    detect_vanished: bool,
) -> Result<FetchStats, SyncError>
where
    M: MailboxSession,
    S: SyncStore,
{
    let headers = session.uid_fetch_headers(folder, range)?;
    let fetched = headers.len();
    let mut vanished_count = 0;
    if detect_vanished {
        let present: std::collections::HashSet<u32> = headers.iter().map(|h| h.uid).collect();
        let mut vanished = Vec::new();
        if let Some(to) = range.to {
            for uid in range.from..=to {
                if !present.contains(&uid) {
                    vanished.push(uid);
                }
            }
        }
        vanished_count = vanished.len();
        store.upsert_headers(folder, &headers)?;
        if !vanished.is_empty() {
            store.remove_vanished(folder, &vanished)?;
        }
    } else {
        store.upsert_headers(folder, &headers)?;
    }
    Ok(FetchStats {
        fetched,
        vanished: vanished_count,
    })
}

/// Newest-first backfill of existing mail, in [`UID_FETCH_BATCH`]-sized
/// chunks. `state.backfill_floor` is saved after every successful batch.
fn run_backfill<M, S>(
    session: &mut M,
    store: &mut S,
    folder: &str,
    state: &mut FolderSyncState,
    is_cancelled: &dyn Fn() -> bool,
    outcome: &mut SyncOutcome,
) -> Result<(), SyncError>
where
    M: MailboxSession,
    S: SyncStore,
{
    let mut floor = state.backfill_floor.unwrap_or(1);
    while floor > 1 {
        if is_cancelled() {
            outcome.cancelled = true;
            return Ok(());
        }
        let bottom = floor.saturating_sub(UID_FETCH_BATCH).max(1);
        let range = UidRange::bounded(bottom, floor - 1);
        // Never-before-fetched range: nothing can have "vanished" locally yet.
        let stats = fetch_range(session, store, folder, range, false)?;
        outcome.headers_fetched += stats.fetched;
        outcome.vanished_removed += stats.vanished;
        floor = bottom;
        state.backfill_floor = Some(floor);
        store.save_state(folder, state)?;
    }
    if floor <= 1 {
        state.backfill_floor = None;
    }
    Ok(())
}

/// Pull uids `[start, end_exclusive)` — the mail that arrived since the last
/// successful sync — in ascending batches. `state.uidnext` advances (and is
/// saved) only after each batch is durably written.
#[allow(clippy::too_many_arguments)]
fn pull_new_mail<M, S>(
    session: &mut M,
    store: &mut S,
    folder: &str,
    start: u32,
    end_exclusive: u32,
    state: &mut FolderSyncState,
    is_cancelled: &dyn Fn() -> bool,
    outcome: &mut SyncOutcome,
) -> Result<(), SyncError>
where
    M: MailboxSession,
    S: SyncStore,
{
    let mut bottom = start;
    while bottom < end_exclusive {
        if is_cancelled() {
            outcome.cancelled = true;
            return Ok(());
        }
        let top = (bottom + UID_FETCH_BATCH).min(end_exclusive);
        let range = UidRange::bounded(bottom, top - 1);
        // Brand-new UIDs since the last sync: nothing local to vanish either.
        let stats = fetch_range(session, store, folder, range, false)?;
        outcome.headers_fetched += stats.fetched;
        outcome.vanished_removed += stats.vanished;
        bottom = top;
        state.uidnext = Some(bottom);
        store.save_state(folder, state)?;
    }
    Ok(())
}

/// CONDSTORE delta: flags changed since the last known `HIGHESTMODSEQ`.
#[allow(clippy::too_many_arguments)]
fn sync_condstore_flags<M, S>(
    session: &mut M,
    store: &mut S,
    folder: &str,
    since_modseq: u64,
    uidnext: u32,
    is_cancelled: &dyn Fn() -> bool,
    outcome: &mut SyncOutcome,
) -> Result<(), SyncError>
where
    M: MailboxSession,
    S: SyncStore,
{
    if is_cancelled() {
        outcome.cancelled = true;
        return Ok(());
    }
    let top = uidnext.saturating_sub(1).max(1);
    let range = UidRange::bounded(1, top);
    let changed = session.uid_fetch_flags_changed_since(folder, range, since_modseq)?;
    outcome.flags_updated += changed.len();
    store.upsert_headers(folder, &changed)?;
    Ok(())
}

/// Workspace probe so `cargo test -p feathermail-sync` has a test even
/// before the fixtures below.
pub fn crate_name() -> &'static str {
    env!("CARGO_PKG_NAME")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::collections::HashMap;

    #[test]
    fn crate_compiles() {
        assert!(crate_name().starts_with("feathermail-"));
    }

    #[derive(Default)]
    struct FakeSession {
        messages: Vec<HeaderMeta>,
        uidvalidity: u32,
        highest_modseq: u64,
        condstore: bool,
        fetch_call_count: usize,
        fetched_uids_log: Vec<u32>,
        changedsince_log: Vec<u64>,
        fail_on_call: Option<usize>,
        /// Lets a test pin `uidnext` independently of the message set (e.g.
        /// a sparse mailbox: only uids 5/100/900 present but `uidnext` is
        /// 1000, same as a real mailbox with lots of already-expunged uids).
        uidnext_override: Option<u32>,
        /// T-024: canned bodies by uid, for `fetch_body` tests.
        bodies: HashMap<u32, Vec<u8>>,
        /// Every `select`/`fetch_body` call, in order, as e.g.
        /// `"select:INBOX"` / `"fetch_body:INBOX:42"` -- proves the free
        /// function [`fetch_body`] actually selects the right folder
        /// before fetching, not just that both calls compile.
        call_log: Vec<String>,
    }

    impl FakeSession {
        fn new(count: u32) -> Self {
            let messages = (1..=count)
                .map(|uid| HeaderMeta {
                    uid,
                    ..HeaderMeta::default()
                })
                .collect();
            Self {
                messages,
                uidvalidity: 1,
                highest_modseq: 0,
                condstore: true,
                ..Self::default()
            }
        }

        fn add_message(&mut self, uid: u32) {
            self.messages.push(HeaderMeta {
                uid,
                ..HeaderMeta::default()
            });
            self.highest_modseq += 1;
        }

        /// Flip a flag on an already-existing message without changing the
        /// message set itself — simulates e.g. a read/unread toggle arriving
        /// while an unrelated backfill is still in progress.
        fn change_flag(&mut self, uid: u32, flag: &str) {
            if let Some(m) = self.messages.iter_mut().find(|m| m.uid == uid) {
                m.flags.push(flag.to_string());
            }
            self.highest_modseq += 1;
        }

        fn snapshot(&self) -> MailboxSnapshot {
            let uidnext = self.uidnext_override.unwrap_or_else(|| {
                self.messages
                    .iter()
                    .map(|m| m.uid)
                    .max()
                    .map_or(1, |m| m + 1)
            });
            MailboxSnapshot {
                uidvalidity: self.uidvalidity,
                uidnext,
                exists: self.messages.len() as u32,
                highest_modseq: self.condstore.then_some(self.highest_modseq),
            }
        }
    }

    impl MailboxSession for FakeSession {
        fn select(&mut self, folder: &str) -> Result<MailboxSnapshot, SyncError> {
            self.call_log.push(format!("select:{folder}"));
            Ok(self.snapshot())
        }

        fn uid_fetch_headers(
            &mut self,
            _folder: &str,
            range: UidRange,
        ) -> Result<Vec<HeaderMeta>, SyncError> {
            self.fetch_call_count += 1;
            if self.fail_on_call == Some(self.fetch_call_count) {
                return Err(SyncError::Session("simulated network drop".into()));
            }
            let to = range.to.unwrap_or(u32::MAX);
            let result: Vec<HeaderMeta> = self
                .messages
                .iter()
                .filter(|m| m.uid >= range.from && m.uid <= to)
                .cloned()
                .collect();
            self.fetched_uids_log.extend(result.iter().map(|m| m.uid));
            Ok(result)
        }

        fn uid_fetch_flags_changed_since(
            &mut self,
            _folder: &str,
            range: UidRange,
            modseq: u64,
        ) -> Result<Vec<HeaderMeta>, SyncError> {
            self.changedsince_log.push(modseq);
            let to = range.to.unwrap_or(u32::MAX);
            Ok(self
                .messages
                .iter()
                .filter(|m| m.uid >= range.from && m.uid <= to)
                .cloned()
                .collect())
        }

        fn list_folders(&mut self) -> Result<Vec<String>, SyncError> {
            Ok(vec!["INBOX".into()])
        }

        fn fetch_body(&mut self, folder: &str, uid: u32) -> Result<Vec<u8>, SyncError> {
            self.call_log.push(format!("fetch_body:{folder}:{uid}"));
            self.bodies
                .get(&uid)
                .cloned()
                .ok_or_else(|| SyncError::Session(format!("no such uid {uid}")))
        }
    }

    #[derive(Default)]
    struct FakeStore {
        state: HashMap<String, FolderSyncState>,
        rows: HashMap<String, HashMap<u32, HeaderMeta>>,
        reset_calls: usize,
        vanished_calls: usize,
    }

    impl SyncStore for FakeStore {
        fn load_state(&mut self, folder: &str) -> Result<FolderSyncState, SyncError> {
            Ok(self.state.get(folder).cloned().unwrap_or_default())
        }

        fn save_state(&mut self, folder: &str, state: &FolderSyncState) -> Result<(), SyncError> {
            self.state.insert(folder.to_string(), state.clone());
            Ok(())
        }

        fn upsert_headers(
            &mut self,
            folder: &str,
            headers: &[HeaderMeta],
        ) -> Result<(), SyncError> {
            let table = self.rows.entry(folder.to_string()).or_default();
            for h in headers {
                table.insert(h.uid, h.clone());
            }
            Ok(())
        }

        fn remove_vanished(&mut self, folder: &str, uids: &[u32]) -> Result<(), SyncError> {
            self.vanished_calls += 1;
            if let Some(table) = self.rows.get_mut(folder) {
                for uid in uids {
                    table.remove(uid);
                }
            }
            Ok(())
        }

        fn reset_folder(&mut self, folder: &str) -> Result<(), SyncError> {
            self.reset_calls += 1;
            self.rows.remove(folder);
            Ok(())
        }
    }

    fn no_cancel() -> impl Fn() -> bool {
        || false
    }

    #[test]
    fn second_run_fetches_only_the_delta() {
        let mut session = FakeSession::new(10);
        let mut store = FakeStore::default();
        let cancel = no_cancel();

        let out1 = sync_folder(&mut session, &mut store, "INBOX", 1, &cancel).unwrap();
        assert_eq!(out1.headers_fetched, 10);
        assert_eq!(store.rows["INBOX"].len(), 10);

        session.add_message(11);
        session.add_message(12);
        session.add_message(13);

        let out2 = sync_folder(&mut session, &mut store, "INBOX", 2, &cancel).unwrap();
        assert_eq!(
            out2.headers_fetched, 3,
            "second run must fetch exactly the 3 new messages"
        );
        assert!(out2.headers_fetched < out1.headers_fetched);
        assert_eq!(store.rows["INBOX"].len(), 13);

        // A third, no-op run fetches nothing at all.
        let out3 = sync_folder(&mut session, &mut store, "INBOX", 3, &cancel).unwrap();
        assert_eq!(out3.headers_fetched, 0);
    }

    #[test]
    fn uidvalidity_change_forces_full_resync() {
        let mut session = FakeSession::new(5);
        let mut store = FakeStore::default();
        let cancel = no_cancel();

        sync_folder(&mut session, &mut store, "INBOX", 1, &cancel).unwrap();
        assert_eq!(store.rows["INBOX"].len(), 5);

        session.uidvalidity = 999;
        session.messages = (1..=5)
            .map(|uid| HeaderMeta {
                uid,
                ..HeaderMeta::default()
            })
            .collect();

        let out2 = sync_folder(&mut session, &mut store, "INBOX", 2, &cancel).unwrap();
        assert!(out2.uidvalidity_reset);
        assert_eq!(
            out2.headers_fetched, 5,
            "uidvalidity bump must re-fetch every uid"
        );
        assert_eq!(store.reset_calls, 1);
    }

    #[test]
    fn condstore_path_uses_changed_since() {
        let mut session = FakeSession::new(3);
        let mut store = FakeStore::default();
        let cancel = no_cancel();

        sync_folder(&mut session, &mut store, "INBOX", 1, &cancel).unwrap();
        session.highest_modseq += 5;

        sync_folder(&mut session, &mut store, "INBOX", 2, &cancel).unwrap();
        assert_eq!(session.changedsince_log.len(), 1);
        assert_eq!(
            session.changedsince_log[0], 0,
            "must pass the OLD modseq, not the new one"
        );
    }

    #[test]
    fn network_drop_mid_backfill_resumes_without_gap_or_redownload() {
        let mut session = FakeSession::new(500);
        session.fail_on_call = Some(2);
        let mut store = FakeStore::default();
        let cancel = no_cancel();

        let err = sync_folder(&mut session, &mut store, "INBOX", 1, &cancel).unwrap_err();
        assert!(matches!(err, SyncError::Session(_)));

        let stored_after_failure = store.rows["INBOX"].len();
        assert!(stored_after_failure > 0 && stored_after_failure < 500);

        session.fail_on_call = None;
        let out2 = sync_folder(&mut session, &mut store, "INBOX", 2, &cancel).unwrap();
        assert_eq!(out2.headers_fetched, 500 - stored_after_failure);
        assert_eq!(store.rows["INBOX"].len(), 500);

        // No gaps and no duplicated download: every uid 1..=500 present exactly once.
        for uid in 1..=500u32 {
            assert!(store.rows["INBOX"].contains_key(&uid), "missing uid {uid}");
        }
    }

    #[test]
    fn cancellation_stops_between_batches_and_resumes() {
        let mut session = FakeSession::new(500);
        let mut store = FakeStore::default();
        let calls = Cell::new(0u32);
        let cancel_after_first_batch = || {
            let n = calls.get();
            calls.set(n + 1);
            n >= 1
        };

        let out1 = sync_folder(
            &mut session,
            &mut store,
            "INBOX",
            1,
            &cancel_after_first_batch,
        )
        .unwrap();
        assert!(out1.cancelled);
        assert!(out1.headers_fetched > 0 && out1.headers_fetched < 500);
        assert_eq!(store.rows["INBOX"].len(), out1.headers_fetched);

        let cancel = no_cancel();
        let out2 = sync_folder(&mut session, &mut store, "INBOX", 2, &cancel).unwrap();
        assert!(!out2.cancelled);
        assert_eq!(store.rows["INBOX"].len(), 500);
    }

    #[test]
    fn resync_flags_range_removes_vanished_mail() {
        let mut session = FakeSession::new(5);
        let mut store = FakeStore::default();
        let cancel = no_cancel();
        sync_folder(&mut session, &mut store, "INBOX", 1, &cancel).unwrap();
        assert_eq!(store.rows["INBOX"].len(), 5);

        session.messages.retain(|m| m.uid != 3);
        let out =
            resync_flags_range(&mut session, &mut store, "INBOX", UidRange::bounded(1, 5)).unwrap();
        assert_eq!(out.vanished_removed, 1);
        assert_eq!(out.checked, 5);
        assert!(!store.rows["INBOX"].contains_key(&3));
    }

    #[test]
    fn empty_mailbox_first_run_fetches_nothing() {
        let mut session = FakeSession::new(0);
        let mut store = FakeStore::default();
        let cancel = no_cancel();
        let out = sync_folder(&mut session, &mut store, "INBOX", 1, &cancel).unwrap();
        assert_eq!(out.headers_fetched, 0);
        assert!(!out.cancelled);
        let state = store.load_state("INBOX").unwrap();
        assert_eq!(state.backfill_floor, None);
        assert_eq!(state.uidnext, Some(1));
    }

    // --- Regression coverage for the two data-loss bugs from review -------

    #[test]
    fn interrupted_backfill_with_new_mail_between_runs_has_no_gaps() {
        let mut session = FakeSession::new(1000);
        let mut store = FakeStore::default();
        let cancel = no_cancel();

        // Let the first backfill batch land, then simulate a dropped
        // connection on the second.
        session.fail_on_call = Some(2);
        let err = sync_folder(&mut session, &mut store, "INBOX", 1, &cancel).unwrap_err();
        assert!(matches!(err, SyncError::Session(_)));
        session.fail_on_call = None;

        let stored_before = store.rows.get("INBOX").map_or(0, |t| t.len());
        assert!(
            stored_before > 0 && stored_before < 1000,
            "backfill must have made partial, durable progress"
        );

        // New mail arrives while the backfill is still incomplete.
        session.add_message(1001);
        session.add_message(1002);

        // This run must both finish the backfill AND pick up the new mail
        // that arrived in between — in the same pass, since a bug fixed here
        // let an `else if` skip the new-mail pull whenever a backfill was
        // also completing.
        let out = sync_folder(&mut session, &mut store, "INBOX", 2, &cancel).unwrap();
        assert!(!out.cancelled);

        let rows = &store.rows["INBOX"];
        for uid in 1..=1002u32 {
            assert!(rows.contains_key(&uid), "gap at uid {uid}: never synced");
        }
        assert_eq!(rows.len(), 1002);

        let state = store.load_state("INBOX").unwrap();
        assert_eq!(
            state.uidnext,
            Some(1003),
            "cursor must reflect what was actually fetched, not a blindly-adopted snapshot value"
        );
    }

    #[test]
    fn interrupted_backfill_does_not_advance_modseq_until_delta_is_applied() {
        let mut session = FakeSession::new(500);
        let mut store = FakeStore::default();
        let cancel = no_cancel();

        session.fail_on_call = Some(2);
        let _ = sync_folder(&mut session, &mut store, "INBOX", 1, &cancel).unwrap_err();
        session.fail_on_call = None;

        let state_after_failure = store.load_state("INBOX").unwrap();
        assert_eq!(
            state_after_failure.highest_modseq,
            Some(0),
            "the baseline captured at first contact must not have moved"
        );

        // Flip a flag on a message that was already backfilled in the first
        // (successful) batch, while the backfill for older mail is still
        // incomplete.
        session.change_flag(480, "\\Seen");
        assert_eq!(session.highest_modseq, 1);

        let out = sync_folder(&mut session, &mut store, "INBOX", 2, &cancel).unwrap();
        assert!(!out.cancelled);
        assert_eq!(
            session.changedsince_log,
            vec![0],
            "CHANGEDSINCE must be issued with the OLD modseq, not a newer one"
        );

        let state_after = store.load_state("INBOX").unwrap();
        assert_eq!(
            state_after.highest_modseq,
            Some(1),
            "modseq only advances once the delta for this exact window was fetched"
        );
    }

    #[test]
    fn cancellation_inside_condstore_flags_does_not_advance_modseq() {
        let mut session = FakeSession::new(5);
        let mut store = FakeStore::default();
        let cancel = no_cancel();
        sync_folder(&mut session, &mut store, "INBOX", 1, &cancel).unwrap();

        session.highest_modseq += 3;
        let cancel_now = || true;
        let out = sync_folder(&mut session, &mut store, "INBOX", 2, &cancel_now).unwrap();
        assert!(out.cancelled);
        assert!(
            session.changedsince_log.is_empty(),
            "cancelled before the CHANGEDSINCE fetch happened"
        );

        let state = store.load_state("INBOX").unwrap();
        assert_eq!(
            state.highest_modseq,
            Some(0),
            "must not adopt the new modseq without ever applying the delta"
        );
    }

    #[test]
    fn first_backfill_of_sparse_mailbox_never_calls_remove_vanished() {
        let mut session = FakeSession::new(0);
        session.messages = [5u32, 100, 900]
            .into_iter()
            .map(|uid| HeaderMeta {
                uid,
                ..HeaderMeta::default()
            })
            .collect();
        session.uidnext_override = Some(1000);
        let mut store = FakeStore::default();
        let cancel = no_cancel();

        let out = sync_folder(&mut session, &mut store, "INBOX", 1, &cancel).unwrap();
        assert_eq!(out.headers_fetched, 3);
        assert_eq!(out.vanished_removed, 0);
        assert_eq!(
            store.vanished_calls, 0,
            "first backfill must never attempt to delete rows that were never stored"
        );
        assert_eq!(store.rows["INBOX"].len(), 3);
    }

    #[test]
    fn repeat_reconciliation_still_removes_a_message_that_really_vanished() {
        // Same non-contiguous mailbox as above (backfill must skip vanished
        // detection there), but a *second* pass — `resync_flags_range` over a
        // range the store already has real rows for — must still catch a
        // message that genuinely disappeared.
        let mut session = FakeSession::new(0);
        session.messages = [5u32, 100, 900]
            .into_iter()
            .map(|uid| HeaderMeta {
                uid,
                ..HeaderMeta::default()
            })
            .collect();
        session.uidnext_override = Some(1000);
        let mut store = FakeStore::default();
        let cancel = no_cancel();
        sync_folder(&mut session, &mut store, "INBOX", 1, &cancel).unwrap();
        assert_eq!(store.rows["INBOX"].len(), 3);

        session.messages.retain(|m| m.uid != 100);
        // Tight range around the message that actually vanished, as a real
        // reconciliation pass would use (it checks known message
        // neighborhoods, not the whole sparse UID space).
        let out = resync_flags_range(
            &mut session,
            &mut store,
            "INBOX",
            UidRange::bounded(100, 100),
        )
        .unwrap();
        assert_eq!(out.checked, 1);
        assert_eq!(out.vanished_removed, 1);
        assert!(store.vanished_calls >= 1);
        assert!(!store.rows["INBOX"].contains_key(&100));
    }

    #[test]
    fn backfill_that_ends_between_saves_still_pulls_new_mail_later() {
        // The batch loop persists `backfill_floor: Some(1)` after its last
        // batch; if the run then dies before the end-of-run save (here: the
        // new-mail fetch drops), that spelling is what the next run loads.
        // It must be read as "backfill done", not "still pending" — otherwise
        // the folder never advances its cursor and never sees mail again.
        // CONDSTORE off so nothing else can quietly repopulate the rows.
        let mut session = FakeSession::new(1000);
        session.condstore = false;
        let mut store = FakeStore::default();
        let cancel = no_cancel();

        session.fail_on_call = Some(2);
        let _ = sync_folder(&mut session, &mut store, "INBOX", 1, &cancel).unwrap_err();
        session.fail_on_call = None;

        session.add_message(1001);
        session.add_message(1002);

        // Run 2 finishes the backfill (calls 3..=6), then dies on the first
        // new-mail fetch (call 7), leaving `backfill_floor: Some(1)` on disk.
        session.fail_on_call = Some(7);
        let _ = sync_folder(&mut session, &mut store, "INBOX", 2, &cancel).unwrap_err();
        session.fail_on_call = None;
        assert_eq!(
            store.load_state("INBOX").unwrap().backfill_floor,
            Some(1),
            "precondition: the run left the exhausted-floor spelling on disk"
        );

        let out = sync_folder(&mut session, &mut store, "INBOX", 3, &cancel).unwrap();
        assert_eq!(out.headers_fetched, 2, "the two new uids must finally land");

        let rows = &store.rows["INBOX"];
        for uid in 1..=1002u32 {
            assert!(rows.contains_key(&uid), "gap at uid {uid}");
        }
        let state = store.load_state("INBOX").unwrap();
        assert_eq!(state.backfill_floor, None);
        assert_eq!(state.backfill_target, None);
        assert_eq!(state.uidnext, Some(1003));
    }

    /// T-027 follow-up (coordinator review): could a CONDSTORE flags-only
    /// delta reach the store for a UID the backfill has not synced yet,
    /// while `backfill_floor` is still pending? If so, that UID would get
    /// upserted with a flags-only (content-empty) `HeaderMeta` through the
    /// *insert* path in `CoreSyncStore`, indistinguishable from genuine
    /// corruption.
    ///
    /// Trace through `sync_folder`: the CONDSTORE step is gated by
    /// `!outcome.cancelled`, same as the new-mail pull right above it.
    /// `run_backfill(..)?` either (a) returns `Err`, which the `?`
    /// propagates straight out of `sync_folder` -- the whole call aborts
    /// before CONDSTORE is ever reached; or (b) is cancelled mid-loop,
    /// which sets `outcome.cancelled = true` and is returned `Ok` --
    /// blocking CONDSTORE via the same flag; or (c) drains the `while
    /// floor > 1` loop to completion (its only other exit), which sets
    /// `state.backfill_floor = None` before returning. So on every path
    /// that reaches CONDSTORE in a given call, `backfill_floor` is *already*
    /// `None` for that exact call — the worried interleaving cannot occur.
    /// This test forces the closest possible approximation of it (a flag
    /// change deep in the still-unbackfilled range, then a run that keeps
    /// getting cancelled) and confirms CONDSTORE is never even queried
    /// until backfill has actually finished.
    #[test]
    fn condstore_never_fires_while_backfill_is_still_pending_even_if_flags_changed_on_an_unsynced_uid(
    ) {
        let mut session = FakeSession::new(1000);
        let mut store = FakeStore::default();

        // Run 1: let exactly one backfill batch land, then cancel. Backfill
        // goes newest-first, so uids 1..=800 (including the low uid 5 used
        // below) remain unbackfilled.
        let calls = Cell::new(0u32);
        let cancel_after_first_batch = || {
            let n = calls.get();
            calls.set(n + 1);
            n >= 1
        };
        let out1 = sync_folder(
            &mut session,
            &mut store,
            "INBOX",
            1,
            &cancel_after_first_batch,
        )
        .unwrap();
        assert!(out1.cancelled);
        let state1 = store.load_state("INBOX").unwrap();
        assert!(
            state1.backfill_floor.is_some_and(|f| f > 1),
            "precondition: backfill must still be pending"
        );

        // A flag changes on uid 5, which has no local row at all yet.
        session.change_flag(5, "\\Seen");
        assert!(
            !store.rows["INBOX"].contains_key(&5),
            "precondition: uid 5 has no row yet"
        );

        // Run 2: cancel on the very first poll (inside `run_backfill`) and
        // allow every poll after that. This is deliberately a *single-shot*
        // cancel rather than an always-true one: `sync_condstore_flags` has
        // its own internal `is_cancelled()` check as a second line of
        // defense, and an always-true closure would make that inner check
        // mask a broken outer gate, leaving this test unable to tell the two
        // defenses apart. A single-shot cancel means that if the outer gate
        // in `sync_folder` (the one that is supposed to keep CONDSTORE from
        // running this run at all) were ever removed, `sync_condstore_flags`
        // would be reached with `is_cancelled()` now returning `false` --
        // and would actually run, exposing exactly the bug the coordinator
        // was worried about.
        let cancelled_once = Cell::new(false);
        let cancel_first_poll_only = || {
            if cancelled_once.get() {
                false
            } else {
                cancelled_once.set(true);
                true
            }
        };
        let out2 = sync_folder(
            &mut session,
            &mut store,
            "INBOX",
            2,
            &cancel_first_poll_only,
        )
        .unwrap();
        assert!(out2.cancelled);
        assert!(
            session.changedsince_log.is_empty(),
            "CONDSTORE must not be queried at all while this run's backfill is still pending"
        );
        assert!(
            !store.rows["INBOX"].contains_key(&5),
            "uid 5 must still have no row: it cannot have been upserted via a flags-only delta"
        );

        // Run 3: let the backfill fully finish. Only now can CONDSTORE run,
        // and by then uid 5 already has a real row from the backfill's own
        // full header fetch, not from a flags-only delta.
        let out3 = sync_folder(&mut session, &mut store, "INBOX", 3, &no_cancel()).unwrap();
        assert!(!out3.cancelled);
        let state3 = store.load_state("INBOX").unwrap();
        assert_eq!(
            state3.backfill_floor, None,
            "precondition for CONDSTORE to run at all: backfill must be fully done"
        );
        assert!(store.rows["INBOX"].contains_key(&5));
    }

    // --- T-027: duplicate Message-Id / corrupted-content fixtures ---------
    //
    // The engine (this crate) never interprets a `HeaderMeta`'s content --
    // that happens downstream, in whatever `SyncStore` the caller supplies
    // (see `feathermail_core::sync_store` for the real one). What belongs
    // here is proving the *batch machinery* — `fetch_range`, the backfill
    // loop, cursor bookkeeping — does not care what a header contains, and
    // does not choke when a server repeats a UID within one response.

    #[test]
    fn duplicate_uid_within_one_fetch_response_does_not_crash_and_upserts_once() {
        // A server bug (or a retried/duplicated response) can hand back the
        // same UID twice in a single `UID FETCH` reply. The engine must
        // still complete the pass, and the upsert-by-uid store contract
        // must collapse the repeat to one row rather than let it become two
        // or error out.
        let mut session = FakeSession::new(0);
        session.messages = vec![
            HeaderMeta {
                uid: 1,
                subject: Some("First".to_string()),
                ..HeaderMeta::default()
            },
            HeaderMeta {
                uid: 1,
                subject: Some("Repeat".to_string()),
                ..HeaderMeta::default()
            },
            HeaderMeta {
                uid: 2,
                ..HeaderMeta::default()
            },
        ];
        let mut store = FakeStore::default();
        let cancel = no_cancel();

        let out = sync_folder(&mut session, &mut store, "INBOX", 1, &cancel).unwrap();
        assert!(!out.cancelled);
        assert_eq!(
            store.rows["INBOX"].len(),
            2,
            "uid 1's repeat within the same batch must collapse, not double up"
        );
        assert_eq!(
            store.rows["INBOX"][&1].subject.as_deref(),
            Some("Repeat"),
            "the later entry in the same fetch response wins"
        );

        // A second, no-op pass must not lose or duplicate anything either.
        let out2 = sync_folder(&mut session, &mut store, "INBOX", 2, &cancel).unwrap();
        assert_eq!(out2.headers_fetched, 0);
        assert_eq!(store.rows["INBOX"].len(), 2);
    }

    #[test]
    fn a_header_carrying_no_content_at_all_flows_through_a_batch_like_any_other_message() {
        // Stand-in for what a garbled RFC822 header block degrades to once
        // the (out-of-scope, providers-crate) wire parser gives up on it: a
        // well-typed `HeaderMeta` with nothing but a uid. Mixed into an
        // otherwise-normal batch, it must not stop the batch or the ones
        // after it from being fetched and saved.
        let mut session = FakeSession::new(5);
        session.messages[2] = HeaderMeta {
            uid: 3,
            ..HeaderMeta::default()
        };
        let mut store = FakeStore::default();
        let cancel = no_cancel();

        let out = sync_folder(&mut session, &mut store, "INBOX", 1, &cancel).unwrap();
        assert!(!out.cancelled);
        assert_eq!(
            out.headers_fetched, 5,
            "the corrupted message must not stop the pass"
        );
        for uid in 1..=5u32 {
            assert!(
                store.rows["INBOX"].contains_key(&uid),
                "uid {uid} missing after a batch containing one corrupted message"
            );
        }
    }

    #[test]
    fn fetch_body_selects_the_folder_before_fetching() {
        let mut session = FakeSession::new(1);
        session.bodies.insert(42, b"hello world".to_vec());

        let bytes = fetch_body(&mut session, "INBOX", 42).unwrap();

        assert_eq!(bytes, b"hello world");
        assert_eq!(
            session.call_log,
            vec![
                "select:INBOX".to_string(),
                "fetch_body:INBOX:42".to_string()
            ],
            "select must run before fetch_body, for the folder actually asked for"
        );
    }

    #[test]
    fn fetch_body_propagates_the_session_error_for_an_unknown_uid() {
        let mut session = FakeSession::new(1);
        let err = fetch_body(&mut session, "INBOX", 999).unwrap_err();
        assert!(matches!(err, SyncError::Session(_)));
    }

    /// Mail deleted on the server (webmail, phone, another client) must stop
    /// existing locally too -- the crate doc's "vanished UIDs are removed
    /// locally" and D29. The only code that can notice a vanished UID is
    /// `fetch_range(.., detect_vanished = true)`; the rest of a pass only
    /// ever walks *new* UIDs and a CONDSTORE flags delta, neither of which
    /// can report mail that is simply gone.
    #[test]
    fn mail_deleted_on_the_server_is_removed_locally_on_the_next_pass() {
        let mut session = FakeSession::new(5);
        let mut store = FakeStore::default();
        let cancel = no_cancel();

        sync_folder(&mut session, &mut store, "INBOX", 1, &cancel).unwrap();
        assert!(
            store.rows["INBOX"].contains_key(&3),
            "precondition: uid 3 was synced by the first pass"
        );

        // The user deletes that message from another client. On the wire the
        // UID is simply gone from every FETCH response afterwards.
        session.messages.retain(|m| m.uid != 3);
        session.highest_modseq += 1;

        sync_folder(&mut session, &mut store, "INBOX", 2, &cancel).unwrap();

        assert!(
            !store.rows["INBOX"].contains_key(&3),
            "a UID the server no longer has must be removed locally, not kept forever"
        );
    }

    /// The reconciliation above must stay *bounded*, not a `1:*` sweep: on
    /// a 100k mailbox one pass may cost the newest window plus one batch of
    /// the rolling walk (T-157) and not one byte more, or every background
    /// pass would re-download every envelope in the folder.
    #[test]
    fn reconciliation_of_a_huge_mailbox_costs_a_window_and_one_walk_batch() {
        let mut session = FakeSession::new(0);
        session.messages = vec![HeaderMeta {
            uid: 99_999,
            ..HeaderMeta::default()
        }];
        session.uidnext_override = Some(100_000);
        let mut store = FakeStore::default();
        // Stand in for a folder whose backfill finished long ago, so this
        // pass has nothing to do but reconcile.
        store.state.insert(
            "INBOX".to_string(),
            FolderSyncState {
                uidvalidity: Some(1),
                uidnext: Some(100_000),
                highest_modseq: Some(0),
                last_synced_at: Some(0),
                backfill_floor: None,
                backfill_target: None,
                resync_cursor: None,
                resync_completed_at: None,
            },
        );
        store.upsert_headers("INBOX", &session.messages).unwrap();

        let out = sync_folder(&mut session, &mut store, "INBOX", 1, &no_cancel()).unwrap();

        assert!(!out.cancelled);
        assert_eq!(
            session.fetch_call_count, 2,
            "one window plus one walk batch, not one fetch per {UID_FETCH_BATCH} \
             uids in the whole mailbox"
        );
        assert!(
            store.rows["INBOX"].contains_key(&99_999),
            "the one message the server still has must survive the window"
        );
        // Ten more passes stay at the same price: the walk takes one batch
        // each, never a catch-up sweep for the passes it has not reached.
        for now in 2..=11 {
            sync_folder(&mut session, &mut store, "INBOX", now, &no_cancel()).unwrap();
        }
        assert_eq!(session.fetch_call_count, 22);
    }

    /// D30: a server that reports no `HIGHESTMODSEQ` never sends a flags
    /// delta, so the whole synced range has to be re-read from time to time
    /// -- but only from time to time, never on every pass.
    #[test]
    fn a_server_without_condstore_gets_a_whole_range_pass_only_now_and_then() {
        let mut session = FakeSession::new(500);
        session.condstore = false;
        let mut store = FakeStore::default();
        let cancel = no_cancel();

        sync_folder(&mut session, &mut store, "INBOX", 0, &cancel).unwrap();
        assert_eq!(store.rows["INBOX"].len(), 500);

        // A message far below the newest window disappears on the server.
        session.messages.retain(|m| m.uid != 3);

        sync_folder(&mut session, &mut store, "INBOX", 1, &cancel).unwrap();
        assert!(
            store.rows["INBOX"].contains_key(&3),
            "a pass one second later must stay a cheap window, not sweep the folder"
        );

        let out = sync_folder(
            &mut session,
            &mut store,
            "INBOX",
            1 + FULL_RECONCILE_INTERVAL_SECS,
            &cancel,
        )
        .unwrap();
        assert_eq!(out.vanished_removed, 1);
        assert!(
            !store.rows["INBOX"].contains_key(&3),
            "the periodic whole-range pass must catch mail the window cannot reach"
        );
    }

    /// The counterpart: with CONDSTORE the flags delta already covers the
    /// whole mailbox on every pass, so the expensive whole-range sweep must
    /// never be taken -- however long it has been since the last pass. What
    /// the pass may spend below the newest window is one rolling-walk batch
    /// (T-157) and nothing more.
    #[test]
    fn a_condstore_server_never_takes_the_whole_range_pass() {
        let mut session = FakeSession::new(500);
        let mut store = FakeStore::default();
        let cancel = no_cancel();

        sync_folder(&mut session, &mut store, "INBOX", 0, &cancel).unwrap();
        let after_backfill = session.fetch_call_count;

        sync_folder(
            &mut session,
            &mut store,
            "INBOX",
            10 * FULL_RECONCILE_INTERVAL_SECS,
            &cancel,
        )
        .unwrap();

        assert_eq!(
            session.fetch_call_count - after_backfill,
            2,
            "CONDSTORE covers flags for free; the pass may re-read the newest \
             window and take one walk batch, not sweep 500 uids"
        );
    }

    /// T-157: the hole the newest window leaves. On a CONDSTORE server the
    /// whole-range sweep is never taken (the test above), the flags delta
    /// reports changed flags rather than gone mail, and the window only
    /// covers the top of the mailbox -- so before the rolling walk existed,
    /// a message deleted from another client deep in the mailbox stayed
    /// local *forever*. It must now disappear on its own, within one circle
    /// of the walk and without any pass sweeping the folder.
    #[test]
    fn mail_deleted_deep_in_a_condstore_mailbox_vanishes_once_the_walk_reaches_it() {
        let mut session = FakeSession::new(500);
        let mut store = FakeStore::default();
        let cancel = no_cancel();

        sync_folder(&mut session, &mut store, "INBOX", 0, &cancel).unwrap();
        assert!(store.rows["INBOX"].contains_key(&3));
        assert!(
            session.snapshot().highest_modseq.is_some(),
            "precondition: this is the CONDSTORE path"
        );

        // Deleted from webmail. Uid 3 is 200+ uids below the newest window.
        session.messages.retain(|m| m.uid != 3);
        session.highest_modseq += 1;

        sync_folder(&mut session, &mut store, "INBOX", 1, &cancel).unwrap();
        assert!(
            store.rows["INBOX"].contains_key(&3),
            "the newest window cannot reach uid 3; this pass only opens the walk"
        );
        assert_eq!(
            store.state["INBOX"].resync_cursor,
            Some(100),
            "the walk checked 101..300 and parked just below it"
        );

        let out = sync_folder(&mut session, &mut store, "INBOX", 2, &cancel).unwrap();
        assert_eq!(out.vanished_removed, 1);
        assert!(
            !store.rows["INBOX"].contains_key(&3),
            "a walk that reached the bottom must have noticed the deletion"
        );
        assert_eq!(
            (
                store.state["INBOX"].resync_cursor,
                store.state["INBOX"].resync_completed_at
            ),
            (None, Some(2)),
            "a completed circle clears the cursor and stamps when it closed"
        );
    }

    /// A closed circle does not immediately start another one: the walk is
    /// a background sweep, not a busy loop, so it waits the same
    /// `FULL_RECONCILE_INTERVAL_SECS` D30 gives the whole-range pass. Until
    /// then a pass costs the newest window and nothing else.
    #[test]
    fn a_finished_walk_waits_out_the_interval_before_starting_the_next_circle() {
        let mut session = FakeSession::new(500);
        let mut store = FakeStore::default();
        let cancel = no_cancel();

        sync_folder(&mut session, &mut store, "INBOX", 0, &cancel).unwrap();
        sync_folder(&mut session, &mut store, "INBOX", 1, &cancel).unwrap();
        sync_folder(&mut session, &mut store, "INBOX", 2, &cancel).unwrap();
        assert_eq!(store.state["INBOX"].resync_completed_at, Some(2));

        let after_circle = session.fetch_call_count;
        sync_folder(&mut session, &mut store, "INBOX", 3, &cancel).unwrap();
        assert_eq!(
            session.fetch_call_count - after_circle,
            1,
            "a pass inside the cool-down may only re-read the newest window"
        );
        assert_eq!(store.state["INBOX"].resync_cursor, None);

        let before_next = session.fetch_call_count;
        sync_folder(
            &mut session,
            &mut store,
            "INBOX",
            2 + FULL_RECONCILE_INTERVAL_SECS,
            &cancel,
        )
        .unwrap();
        assert_eq!(
            session.fetch_call_count - before_next,
            2,
            "once the interval is up the next circle opens, one batch at a time"
        );
        assert_eq!(store.state["INBOX"].resync_cursor, Some(100));
    }

    /// The cursor is state, not a run-local variable: a restart between two
    /// passes must resume the walk where it stopped instead of starting the
    /// circle over from the top (which, on a client restarted often, would
    /// mean the bottom of the mailbox is never checked at all). Here the
    /// engine is handed a store that only knows what was durably saved --
    /// the same thing `CoreSyncStore` reads back out of `sync_state`.
    #[test]
    fn the_walk_resumes_from_the_saved_cursor_after_a_restart() {
        let mut session = FakeSession::new(500);
        let mut store = FakeStore::default();
        let cancel = no_cancel();

        sync_folder(&mut session, &mut store, "INBOX", 0, &cancel).unwrap();
        sync_folder(&mut session, &mut store, "INBOX", 1, &cancel).unwrap();
        let saved = store.state["INBOX"].clone();
        assert_eq!(saved.resync_cursor, Some(100));

        // Restart: a brand new store that has been given nothing but the
        // saved state and the rows the previous run wrote.
        let mut restarted = FakeStore {
            rows: store.rows.clone(),
            ..FakeStore::default()
        };
        restarted.state.insert("INBOX".to_string(), saved);
        session.messages.retain(|m| m.uid != 3);
        session.fetched_uids_log.clear();

        sync_folder(&mut session, &mut restarted, "INBOX", 2, &cancel).unwrap();

        assert!(
            !restarted.rows["INBOX"].contains_key(&3),
            "the resumed walk must pick up at uid 100, not at the window again"
        );
        assert!(
            !session.fetched_uids_log.contains(&150),
            "and it must not re-read the batch the pass before the restart did"
        );
    }

    /// A pass cancelled between batches (T-118: a click yields the socket)
    /// saved its progress but did **not** finish syncing the folder, so it
    /// must not stamp `last_synced_at` -- that column is exactly
    /// `next_sync`'s "enough time has passed since the last *successful*
    /// sync" input, and moving it forward parks a half-done backfill for a
    /// whole scheduler interval.
    #[test]
    fn cancelled_pass_does_not_stamp_last_synced_at() {
        let mut session = FakeSession::new(1000);
        let mut store = FakeStore::default();
        let calls = Cell::new(0u32);
        // First poll (before the first backfill batch) says "keep going";
        // every later one yields, exactly like a FetchBody arriving mid-pass.
        let cancel = || {
            let n = calls.get();
            calls.set(n + 1);
            n >= 1
        };

        let out = sync_folder(&mut session, &mut store, "INBOX", 777, &cancel).unwrap();

        assert!(out.cancelled, "precondition: the pass really was cancelled");
        let state = store.load_state("INBOX").unwrap();
        assert!(
            state.backfill_floor.is_some_and(|f| f > 1),
            "precondition: the backfill is still outstanding, floor = {:?}",
            state.backfill_floor
        );
        assert_eq!(
            state.last_synced_at, None,
            "a cancelled pass is not a completed sync and must not move last_synced_at"
        );
    }
}
