//! Turns one saved account into a live [`MailSession`] (T-078, T-078 (b)):
//! both the [`MailProvider`] the operation queue drains into, and the
//! [`feathermail_sync::MailboxSession`] the inbound folder sync engine
//! drives.
//!
//! [`ProviderFactory`] is the seam [`crate::worker`]'s background loop is
//! tested against without a network or a system keyring: production
//! wiring is [`ImapProviderFactory`]; `worker.rs`'s tests supply their own
//! implementation instead.
//!
//! D11: nothing in this module may be called from a GTK callback. Its only
//! caller is the background thread [`crate::worker::start`] spawns; the
//! shell talks to that thread through [`crate::worker::SyncHandle`] and
//! [`crate::worker::SyncEvent`], never through [`ProviderFactory`] directly.
//!
//! D14: [`ImapProviderFactory::connect`] reads a secret out of the keyring
//! and hands it straight to [`feathermail_providers::ImapSession::connect`];
//! it never returns the secret, logs it, or puts it in a [`ConnectError`]
//! message.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use feathermail_attachments::TransferEncoding;
use feathermail_core::{
    body::attachment_rel_path, AccountId, ApplyError, AttachmentEncoding, AttachmentId,
    ConnectError, Core, CoreError, CoreSyncStore, DraftId, MailProvider, MessageId, OpKind,
    Operation, Reauthenticate, ReauthingProvider, RemoteLocator,
};
use feathermail_providers::{
    build_draft_message, build_outbox_message, discovered_folders, run_idle_with, send_formatted,
    AttachmentFetchLimits, GoogleClientConfig, GoogleOauth, IdleOutcome, ImapMailProvider,
    ImapSession, LiveHttp, MicrosoftClientConfig, MicrosoftOauth, OauthReauth, SmtpAuth,
    TokenRefresh,
};
use feathermail_security::{LibsecretStore, SecretError, SecretKey, SecretStore};
use feathermail_sync::{sync_folder, SyncError, SyncOutcome};

use crate::connector::ConnectorKind;

/// How often an IDLE round (T-089) re-checks `should_stop` while the
/// server is silent. Mirrors `feathermail_providers::idle`'s own
/// `DEFAULT_POLL_SLICE` (private to that module, so not reusable
/// directly) -- same value, same reasoning: small enough that a
/// `Wake`/`FetchBody`/`Shutdown` sent to the worker during an IDLE round
/// is picked up within a few seconds rather than only at the end of
/// whatever ceiling the round was given, without busy-looping the
/// underlying socket read.
const IDLE_POLL_SLICE: Duration = Duration::from_secs(5);
const MAX_ATTACHMENT_WIRE_BYTES: u64 = 700 * 1024 * 1024;
const MAX_ATTACHMENT_DECODED_BYTES: u64 = 512 * 1024 * 1024;

/// Gives [`crate::worker`]'s background loop both roles of one live
/// connection (T-078 (b)): the operation-queue applier ([`MailProvider`],
/// what `Core::tick_for_account` needs) and the inbound sync engine's
/// session ([`MailboxSession`], what `feathermail_sync::sync_folder`
/// needs). Both are ultimately the same live [`ImapSession`] underneath --
/// see [`ImapMailProvider::mailbox`]'s doc comment (`crates/providers/src/apply.rs`)
/// for why handing that session to `sync_folder` on the side, without
/// going through this same provider, is exactly the bug T-078 (a) already
/// found once.
///
/// Declared here rather than in `feathermail-providers` or
/// `feathermail-sync`: implementing a foreign trait
/// ([`MailboxSession`], owned by `feathermail-sync`) for a foreign type
/// ([`ImapMailProvider`], owned by `feathermail-providers`) is only legal
/// from a crate that owns one side of it, and neither of those two crates
/// does (owning `MailboxSession` and depending on `feathermail-providers`
/// to name `ImapMailProvider` would put a sync-scheduling crate above the
/// provider it is meant to sit under, D9). `MailSession` itself is a new,
/// *local* trait, so the orphan rule only ever applies to the `impl`
/// below, not to defining the trait -- and `feathermail-service` already
/// depends on both crates, making it the one place both sides are in
/// scope together.
pub trait MailSession: MailProvider {
    fn append_sent(&mut self, _mailbox: &str, _message: &[u8]) -> Result<(), ApplyError> {
        Err(ApplyError::Unsupported)
    }

    /// Appends a current compose draft to a discovered server Drafts
    /// mailbox. The optional return is the server UID when it advertised
    /// UIDPLUS; callers must treat `None` as a valid successful APPEND.
    fn append_draft(&mut self, _mailbox: &str, _message: &[u8]) -> Result<Option<u32>, ApplyError> {
        Err(ApplyError::Unsupported)
    }

    /// Lists the account's server mailboxes and converts them to Core's
    /// protocol-agnostic discovery shape. The worker calls this once when
    /// an account still has only its local placeholder Inbox, before it
    /// can schedule the first SELECT/header sync (T-077).
    ///
    /// Test sessions that do not model mailbox discovery may keep the
    /// default empty answer. Production IMAP sessions override it with a
    /// real LIST walk.
    fn discover_folders(
        &mut self,
    ) -> Result<Vec<feathermail_core::remote::DiscoveredFolder>, ConnectError> {
        Ok(Vec::new())
    }

    /// Runs exactly one [`sync_folder`] pass against `folder` through this
    /// session, and is what [`crate::worker`] actually calls (T-078 (b)).
    ///
    /// This is a forwarding method rather than the more obvious
    /// `fn mailbox(&mut self) -> &mut dyn MailboxSession` the worker would
    /// then drive itself, and that is not a style choice: `sync_folder<M:
    /// MailboxSession, S: SyncStore>` takes `&mut M`/`&mut S` with the
    /// implicit `Sized` bound every unconstrained type parameter gets, so
    /// `M` can never be an unsized `dyn MailboxSession` -- there is no way
    /// to call it generically across a trait object. Monomorphizing `M` to
    /// the concrete, `Sized` session type (`ImapSession` here) is only
    /// possible from *inside* the impl that knows what that concrete type
    /// is. The same applies to any other generic entry point that wants a
    /// session (`Core::open_body<M: MailboxSession>`, T-080): each needs
    /// its own forwarding method here, not a `dyn` accessor.
    /// `S` does not have the same problem -- [`CoreSyncStore`] is already
    /// a single concrete type the worker constructs itself via
    /// `Core::sync_store`, so it is taken here directly rather than
    /// through a second trait-object parameter.
    fn sync_one_folder(
        &mut self,
        store: &mut CoreSyncStore<'_>,
        folder: &str,
        now: i64,
        is_cancelled: &dyn Fn() -> bool,
    ) -> Result<SyncOutcome, SyncError>;

    /// Runs [`Core::open_body`] against this session (T-080): the exact
    /// same forwarding shape as [`Self::sync_one_folder`] above, for the
    /// exact same reason -- `Core::open_body<M: MailboxSession>` carries
    /// the same implicit `Sized` bound `sync_folder` does, so it can never
    /// be called through a `&mut dyn MailboxSession`. Only an `impl` that
    /// knows the concrete session type can monomorphize `M`, which is why
    /// this lives here rather than behind a `dyn` accessor -- see this
    /// trait's own doc comment on `sync_one_folder` for the fuller
    /// argument; it applies unchanged.
    fn open_one_body(
        &mut self,
        core: &mut Core,
        id: &MessageId,
        bodies_dir: &Path,
    ) -> Result<Vec<u8>, CoreError>;

    /// Runs [`Core::warm_bodies`] against this session: the warm-up's
    /// batched counterpart to [`Self::open_one_body`], forwarded here for
    /// the identical monomorphization reason.
    ///
    /// The default loops [`Self::open_one_body`] and reports each result,
    /// so a session that has no batching of its own (every test double in
    /// this file) behaves exactly as it did before -- it just costs what
    /// it always cost. The real IMAP session overrides it and the hundred
    /// round trips collapse into a handful.
    fn warm_many_bodies(
        &mut self,
        core: &mut Core,
        ids: &[MessageId],
        bodies_dir: &Path,
    ) -> Vec<(MessageId, bool)> {
        ids.iter()
            .map(|id| {
                let ok = self.open_one_body(core, id, bodies_dir).is_ok();
                (id.clone(), ok)
            })
            .collect()
    }

    /// T-043: runs only on the service worker, using Core's stored IMAP
    /// locator and cache path. Test-only sessions may retain this honest
    /// default rather than adding an unrelated attachment implementation.
    fn download_one_attachment(
        &mut self,
        _core: &mut Core,
        _account_id: &AccountId,
        _attachment_id: &AttachmentId,
        _attachments_dir: &Path,
    ) -> Result<(), CoreError> {
        Err(CoreError::from_code(
            feathermail_core::ErrorCode::OperationNotSupported,
        ))
    }

    /// Runs one IMAP `IDLE` round (or, on a server without `IDLE`, one
    /// honest `NOOP` poll -- D30) against `folder` through this session
    /// (T-089): `SELECT`s `folder` first, so the server's unsolicited
    /// updates are reports about the mailbox the caller actually asked to
    /// watch and not whatever this session happened to have selected
    /// before; then defers to `feathermail_providers::run_idle_with` for
    /// the actual RFC 2177 wait/re-issue-before-29-minutes mechanics.
    ///
    /// `should_stop` is checked between poll slices (`IDLE_POLL_SLICE`),
    /// exactly like [`Self::sync_one_folder`]'s `is_cancelled` -- see
    /// `crate::worker::watch_inbox_for_push`'s doc comment for why the
    /// caller wires `should_stop` to break the round on *any* worker
    /// command (`Wake`/`FetchBody`/`Shutdown`), not only `Shutdown`.
    ///
    /// Same forwarding-method shape as [`Self::sync_one_folder`]/
    /// [`Self::open_one_body`] above and for the identical reason: the
    /// concrete, `Sized` `&mut ImapSession` this needs can only be
    /// produced from inside an `impl` that knows the concrete session
    /// type, never through a `&mut dyn` accessor.
    fn idle_once(
        &mut self,
        folder: &str,
        idle_timeout: Duration,
        should_stop: &mut dyn FnMut() -> bool,
    ) -> Result<IdleRound, ConnectError>;
}

/// What one [`MailSession::idle_once`] round found (T-089): the outcome
/// itself, plus whether this server actually supports `IDLE` at all --
/// `crate::worker::watch_inbox_for_push` needs that second bit to decide
/// whether a `TimedOut` round (nothing changed) can go straight back into
/// another IDLE wait, or -- on the no-IDLE fallback, which is a single
/// immediate `NOOP` round trip, not a wait -- must pace itself by D30's
/// `NO_IDLE_POLL_SECS` before polling again, the same way
/// `feathermail_providers::idle`'s own module doc comment describes for
/// any caller of the no-IDLE path.
pub struct IdleRound {
    pub idle_capable: bool,
    pub outcome: IdleOutcome,
}

/// Adds SMTP delivery to the same session object the worker already hands
/// to `Core::tick_for_account`. Non-Send operations and every inbound IMAP
/// method are forwarded to `inner`; Send resolves only the durable outbox
/// id through a separate Core handle on the same WAL profile (D9/T-045).
struct SendingSession {
    inner: Box<dyn MailSession>,
    outbox: Core,
    form: feathermail_core::MailboxForm,
    auth: SmtpAuth,
    secrets: Arc<dyn SecretStore>,
    secret_key: SecretKey,
}

impl MailProvider for SendingSession {
    fn apply(&mut self, op: &Operation) -> Result<(), ApplyError> {
        if op.kind == OpKind::SyncDraft {
            return self.sync_draft(op);
        }
        if op.kind != OpKind::Send {
            return self.inner.apply(op);
        }
        let outgoing = self
            .outbox
            .load_outbox(&op.account_id, &op.target_id)
            .map_err(core_err_to_apply)?;
        // SMTP accepted and Sent APPEND both completed before a crash but the
        // queue ACK did not: never resend and never append twice.
        if outgoing.status == "sent" {
            return Ok(());
        }
        // T-113: encode once, use the same bytes for SMTP `DATA` and for the
        // Sent `APPEND`. `Transport::send` would encode a second copy of its
        // own, and on the largest attachment Core accepts (100 MB, so 136 MiB
        // encoded) the two copies plus the source measured a 420 MiB peak --
        // over D4's ceiling for the whole app. The `Message` is dropped as
        // soon as the bytes exist, so the un-encoded attachment does not sit
        // in memory through the upload either.
        let message = build_outbox_message(&outgoing)?;
        let envelope = message.envelope().clone();
        let raw = message.formatted();
        drop(message);
        if outgoing.status == "queued" {
            let secret = self
                .secrets
                .get(&self.secret_key)
                .map_err(|_| ApplyError::Auth)?
                .ok_or(ApplyError::Auth)?;
            send_formatted(&self.form, self.auth, secret.expose(), &envelope, &raw)?;
            // This marker commits immediately after SMTP acceptance. If the
            // subsequent IMAP APPEND fails or the process crashes, retrying
            // starts at APPEND and never sends a duplicate message.
            self.outbox
                .mark_outbox_delivered(&op.account_id, &op.target_id)
                .map_err(core_err_to_apply)?;
        }
        let sent_folder = self
            .outbox
            .sent_remote_folder(&op.account_id)
            .map_err(core_err_to_apply)?
            .ok_or(ApplyError::Network)?;
        self.inner.append_sent(&sent_folder, &raw)?;
        self.outbox
            .mark_outbox_sent(&op.account_id, &op.target_id)
            .map_err(core_err_to_apply)
    }
}

impl SendingSession {
    fn sync_draft(&mut self, op: &Operation) -> Result<(), ApplyError> {
        let revision = op
            .payload
            .parse::<i64>()
            .map_err(|_| ApplyError::Unsupported)?;
        let draft_id = DraftId(op.target_id.clone());
        let Some(draft) = self
            .outbox
            .draft_for_sync(&op.account_id, &draft_id, revision)
            .map_err(core_err_to_apply)?
        else {
            // An edit or discard won the race after this queue row was
            // claimed. It is already superseded/cancelled locally, so ACK
            // without touching the server rather than sending stale text.
            return Ok(());
        };
        let drafts_folder = self
            .outbox
            .drafts_remote_folder(&op.account_id)
            .map_err(core_err_to_apply)?
            .ok_or(ApplyError::Unsupported)?;
        let message = build_draft_message(&draft)?;
        let remote_uid = self
            .inner
            .append_draft(&drafts_folder, &message.formatted())?;
        self.outbox
            .mark_draft_remote_synced(&op.account_id, &draft_id, revision, remote_uid)
            .map_err(core_err_to_apply)?;
        Ok(())
    }
}

impl MailSession for SendingSession {
    fn append_sent(&mut self, mailbox: &str, message: &[u8]) -> Result<(), ApplyError> {
        self.inner.append_sent(mailbox, message)
    }

    fn append_draft(&mut self, mailbox: &str, message: &[u8]) -> Result<Option<u32>, ApplyError> {
        self.inner.append_draft(mailbox, message)
    }

    fn discover_folders(
        &mut self,
    ) -> Result<Vec<feathermail_core::remote::DiscoveredFolder>, ConnectError> {
        self.inner.discover_folders()
    }

    fn sync_one_folder(
        &mut self,
        store: &mut CoreSyncStore<'_>,
        folder: &str,
        now: i64,
        is_cancelled: &dyn Fn() -> bool,
    ) -> Result<SyncOutcome, SyncError> {
        self.inner.sync_one_folder(store, folder, now, is_cancelled)
    }

    fn open_one_body(
        &mut self,
        core: &mut Core,
        id: &MessageId,
        bodies_dir: &Path,
    ) -> Result<Vec<u8>, CoreError> {
        self.inner.open_one_body(core, id, bodies_dir)
    }

    fn download_one_attachment(
        &mut self,
        core: &mut Core,
        account_id: &AccountId,
        attachment_id: &AttachmentId,
        attachments_dir: &Path,
    ) -> Result<(), CoreError> {
        self.inner
            .download_one_attachment(core, account_id, attachment_id, attachments_dir)
    }

    fn idle_once(
        &mut self,
        folder: &str,
        idle_timeout: Duration,
        should_stop: &mut dyn FnMut() -> bool,
    ) -> Result<IdleRound, ConnectError> {
        self.inner.idle_once(folder, idle_timeout, should_stop)
    }
}

fn core_err_to_apply(err: CoreError) -> ApplyError {
    match err.code {
        feathermail_core::ErrorCode::AccountNotFound
        | feathermail_core::ErrorCode::MessageNotFound => ApplyError::NotFound,
        feathermail_core::ErrorCode::AuthRequired
        | feathermail_core::ErrorCode::PermissionDenied => ApplyError::Auth,
        feathermail_core::ErrorCode::NetworkUnavailable => ApplyError::Network,
        feathermail_core::ErrorCode::Conflict => ApplyError::Network,
        feathermail_core::ErrorCode::InvalidArgument
        | feathermail_core::ErrorCode::OperationNotSupported
        | feathermail_core::ErrorCode::AttachmentTooLarge => ApplyError::Unsupported,
    }
}

fn connect_err_to_apply(err: ConnectError) -> ApplyError {
    match err {
        ConnectError::Auth { .. } => ApplyError::Auth,
        ConnectError::Network { .. } => ApplyError::Network,
        ConnectError::Invalid { .. } => ApplyError::Unsupported,
    }
}

impl<L: RemoteLocator> MailSession for ImapMailProvider<L> {
    fn append_sent(&mut self, mailbox: &str, message: &[u8]) -> Result<(), ApplyError> {
        ImapMailProvider::mailbox(self)
            .append_message(mailbox, &["\\Seen"], message)
            .map(|_| ())
            .map_err(connect_err_to_apply)
    }

    fn append_draft(&mut self, mailbox: &str, message: &[u8]) -> Result<Option<u32>, ApplyError> {
        ImapMailProvider::mailbox(self)
            .append_message(mailbox, &["\\Draft"], message)
            .map_err(connect_err_to_apply)
    }

    fn discover_folders(
        &mut self,
    ) -> Result<Vec<feathermail_core::remote::DiscoveredFolder>, ConnectError> {
        let listings = ImapMailProvider::mailbox(self).list_folders()?;
        Ok(discovered_folders(&listings))
    }

    fn sync_one_folder(
        &mut self,
        store: &mut CoreSyncStore<'_>,
        folder: &str,
        now: i64,
        is_cancelled: &dyn Fn() -> bool,
    ) -> Result<SyncOutcome, SyncError> {
        // `ImapMailProvider::mailbox` (not the trait method above) --
        // both reset `selected` the same way, but calling the inherent
        // method directly here gives `sync_folder` the concrete,
        // `Sized` `&mut ImapSession` it needs (see this trait method's
        // own doc comment on why that cannot happen from the worker).
        sync_folder(
            ImapMailProvider::mailbox(self),
            store,
            folder,
            now,
            is_cancelled,
        )
    }

    fn open_one_body(
        &mut self,
        core: &mut Core,
        id: &MessageId,
        bodies_dir: &Path,
    ) -> Result<Vec<u8>, CoreError> {
        // Same door as `sync_one_folder` above: `ImapMailProvider::mailbox`
        // resets the provider's `selected` cache before handing out the
        // session, so an `apply` immediately after this never trusts a
        // `SELECT` this call (or `Core::open_body`'s own folder resolve)
        // may have issued behind its back. See `ImapMailProvider::mailbox`'s
        // doc comment (`crates/providers/src/apply.rs`) for why that reset
        // is load-bearing, not incidental.
        core.open_body(ImapMailProvider::mailbox(self), id, bodies_dir)
    }

    fn warm_many_bodies(
        &mut self,
        core: &mut Core,
        ids: &[MessageId],
        bodies_dir: &Path,
    ) -> Vec<(MessageId, bool)> {
        // Same door, same reset-before-handing-out rule as `open_one_body`
        // right above; see `ImapMailProvider::mailbox`.
        core.warm_bodies(ImapMailProvider::mailbox(self), ids, bodies_dir)
    }

    fn download_one_attachment(
        &mut self,
        core: &mut Core,
        account_id: &AccountId,
        attachment_id: &AttachmentId,
        attachments_dir: &Path,
    ) -> Result<(), CoreError> {
        let target = core.attachment_download_target(account_id, attachment_id)?;
        let relative = attachment_rel_path(attachment_id);
        let destination = attachments_dir.join(&relative);
        if destination.is_file() {
            core.mark_attachment_cached(account_id, attachment_id, &relative, attachments_dir)?;
            return Ok(());
        }
        let encoding = match target.attachment.transfer_encoding {
            AttachmentEncoding::Base64 => TransferEncoding::Base64,
            AttachmentEncoding::QuotedPrintable => TransferEncoding::QuotedPrintable,
            AttachmentEncoding::Identity => TransferEncoding::Identity,
            AttachmentEncoding::Unsupported => {
                return Err(CoreError::from_code(
                    feathermail_core::ErrorCode::OperationNotSupported,
                ));
            }
        };
        let part_path = target.attachment.part_path.as_deref().ok_or_else(|| {
            CoreError::from_code(feathermail_core::ErrorCode::OperationNotSupported)
        })?;
        let session = ImapMailProvider::mailbox(self);
        session
            .select(&target.remote_folder)
            .map_err(CoreError::from)?;
        session
            .uid_fetch_section_to_file(
                target.provider_uid,
                part_path,
                encoding,
                &destination,
                AttachmentFetchLimits {
                    max_wire_bytes: MAX_ATTACHMENT_WIRE_BYTES,
                    max_decoded_bytes: MAX_ATTACHMENT_DECODED_BYTES,
                },
            )
            .map_err(CoreError::from)?;
        core.mark_attachment_cached(account_id, attachment_id, &relative, attachments_dir)
    }

    fn idle_once(
        &mut self,
        folder: &str,
        idle_timeout: Duration,
        should_stop: &mut dyn FnMut() -> bool,
    ) -> Result<IdleRound, ConnectError> {
        // Same door as `sync_one_folder`/`open_one_body` above: reset the
        // provider's `selected` cache and hand out the concrete session.
        // The `select` right below then re-establishes exactly the
        // mailbox this call is meant to watch, on the wire, before IDLE
        // starts -- IMAP only reports mailbox-level updates for whatever
        // is currently selected, so skipping this would let IDLE silently
        // watch the wrong mailbox (or none at all, on a session that had
        // never selected anything).
        let session = ImapMailProvider::mailbox(self);
        session.select(folder)?;
        let caps = session.capabilities()?;
        let idle_capable = caps.idle;
        let outcome = run_idle_with(session, &caps, idle_timeout, IDLE_POLL_SLICE, should_stop)?;
        Ok(IdleRound {
            idle_capable,
            outcome,
        })
    }
}

/// Gives a Gmail/Microsoft account's queue applier T-083's bounded reauth:
/// every method here just reaches [`ReauthingProvider::inner_mut`]
/// and calls straight through to the exact same [`ImapMailProvider<Core>`]
/// impl above -- not a reimplementation, so there stays exactly one place
/// that knows how to drive a folder sync / body fetch / IDLE round over a
/// live session.
///
/// # What does *not* get the reauth retry, and why
///
/// [`MailProvider::apply`] -- the operation-queue path -- is the only
/// caller `ReauthingProvider` retries: on `ApplyError::Auth` it calls
/// `Reauthenticate::reauthenticate` exactly once and swaps `inner` in on
/// success (`feathermail_core::provider`'s doc comment on
/// [`Reauthenticate`] has the full argument for why that bound needs no
/// counter). The three methods below do not get that: they forward
/// straight into `inner_mut()` with no `Auth`-detecting branch at all,
/// and that is not an oversight to "fix later" so much as a fact about
/// their error types today:
///
/// - [`Self::sync_one_folder`] returns [`SyncError`], whose only two
///   variants are `Session(String)` and `Store(String)` -- there is no
///   `Auth` case a `match` could catch. Telling "the access token just
///   expired mid-sync" apart from any other session error would mean
///   pattern-matching the error *string*, which is exactly the
///   server-message-dependent guess this task was told not to make. So:
///   an access token that goes stale partway through an inbound sync
///   keeps failing `SyncError::Session` on this session until a *new*
///   [`ProviderFactory::connect`] call hands the worker a freshly
///   connected session -- it does not self-heal mid-sync the way `apply`
///   now does. That is a real, user-visible gap this change leaves open,
///   not a hidden one.
/// - [`Self::open_one_body`] and [`Self::idle_once`] carry
///   [`CoreError`]/[`ConnectError`], which *do* have an auth-shaped case,
///   but reauthing on it here would still duplicate the "call
///   `Reauthenticate` at most once, never in a loop" bookkeeping
///   `MailProvider::apply` already owns, against a second error type that
///   could drift out of sync with the first. That duplication, not the
///   absence of an `Auth` variant, is why these two also just forward.
///
/// (Separately, and also not fixed here: `worker.rs`'s
/// `run_one_folder_sync` only checks `.is_ok()` on the sync outcome and
/// does not force a reconnect on failure, so even a *fresh* connect after
/// this kind of staleness does not happen automatically today.)
impl<R: Reauthenticate<ImapMailProvider<Core>>> MailSession
    for ReauthingProvider<ImapMailProvider<Core>, R>
{
    fn append_sent(&mut self, mailbox: &str, message: &[u8]) -> Result<(), ApplyError> {
        self.inner_mut().append_sent(mailbox, message)
    }

    fn append_draft(&mut self, mailbox: &str, message: &[u8]) -> Result<Option<u32>, ApplyError> {
        self.inner_mut().append_draft(mailbox, message)
    }

    fn discover_folders(
        &mut self,
    ) -> Result<Vec<feathermail_core::remote::DiscoveredFolder>, ConnectError> {
        self.inner_mut().discover_folders()
    }

    fn sync_one_folder(
        &mut self,
        store: &mut CoreSyncStore<'_>,
        folder: &str,
        now: i64,
        is_cancelled: &dyn Fn() -> bool,
    ) -> Result<SyncOutcome, SyncError> {
        self.inner_mut()
            .sync_one_folder(store, folder, now, is_cancelled)
    }

    fn open_one_body(
        &mut self,
        core: &mut Core,
        id: &MessageId,
        bodies_dir: &Path,
    ) -> Result<Vec<u8>, CoreError> {
        self.inner_mut().open_one_body(core, id, bodies_dir)
    }

    fn download_one_attachment(
        &mut self,
        core: &mut Core,
        account_id: &AccountId,
        attachment_id: &AttachmentId,
        attachments_dir: &Path,
    ) -> Result<(), CoreError> {
        self.inner_mut()
            .download_one_attachment(core, account_id, attachment_id, attachments_dir)
    }

    fn idle_once(
        &mut self,
        folder: &str,
        idle_timeout: Duration,
        should_stop: &mut dyn FnMut() -> bool,
    ) -> Result<IdleRound, ConnectError> {
        self.inner_mut()
            .idle_once(folder, idle_timeout, should_stop)
    }
}

/// Builds a live [`MailSession`] for one saved account. A trait (not a
/// bare function) so [`crate::worker`] can be driven in tests by a fake
/// that never touches the network or the keyring -- see `worker.rs`'s
/// `#[cfg(test)]` module.
pub trait ProviderFactory: Send {
    fn connect(&mut self, account: &AccountId) -> Result<Box<dyn MailSession>, ConnectError>;
}

/// Where [`ImapProviderFactory::connect`] gets a [`TokenRefresh`] for a
/// Gmail/Microsoft account (T-083, third review round): production loads
/// [`GoogleClientConfig`]/[`MicrosoftClientConfig`] from disk/env and wires
/// it to the real [`LiveHttp`], exactly as `connect` always did inline
/// ([`LiveOauthClientSource`] below); a test hands `connect` a
/// fixed-response double instead, with no config file on disk and no HTTP
/// round trip at all -- see [`ImapProviderFactory::for_test`] and this
/// module's `#[cfg(test)]` block.
///
/// Object-safe (one non-generic method per provider) so this can sit
/// behind a `Box<dyn OauthClientSource>` field on [`ImapProviderFactory`]
/// rather than a second generic type parameter threaded through the
/// struct and every method on it -- the same reason [`SecretStore`] sits
/// behind `Arc<dyn SecretStore>` there instead of a `S: SecretStore` type
/// parameter (see that field's own doc comment for the full argument, T-088's
/// `Arc<dyn SecretStore>` precedent in particular). A generic `ImapProviderFactory<S,
/// O>` was the alternative actually considered and rejected: `ProviderFactory::new`'s
/// only call site (`crates/app/src/shell.rs`, off-limits for this task) writes the bare
/// name `ImapProviderFactory` with no turbofish and no type annotation, so
/// making the struct generic would need a type alias or defaults at that
/// call site anyway for zero benefit here -- nothing outside this module
/// ever needs to know which concrete `OauthClientSource`/`SecretStore` a
/// given factory holds.
trait OauthClientSource: Send {
    fn google(&self) -> Result<Box<dyn TokenRefresh>, ConnectError>;
    fn microsoft(&self) -> Result<Box<dyn TokenRefresh>, ConnectError>;
}

/// Production [`OauthClientSource`]: today's `connect()` behavior, moved
/// out unchanged -- load the client config (env/disk, [`OauthError`]'s
/// `?`-conversion into [`ConnectError`] already existed before this
/// change), wire it to [`LiveHttp`], box it. `ImapProviderFactory::new`
/// installs exactly one of these, so a real caller sees no behavior
/// difference from before this seam existed.
struct LiveOauthClientSource;

impl OauthClientSource for LiveOauthClientSource {
    fn google(&self) -> Result<Box<dyn TokenRefresh>, ConnectError> {
        let oauth = GoogleOauth::new(GoogleClientConfig::load()?, LiveHttp);
        Ok(Box::new(oauth))
    }

    fn microsoft(&self) -> Result<Box<dyn TokenRefresh>, ConnectError> {
        let oauth = MicrosoftOauth::new(MicrosoftClientConfig::load()?, LiveHttp);
        Ok(Box::new(oauth))
    }
}

/// Production [`ProviderFactory`]: reads the account's saved connection
/// settings ([`Core::account_connection`]) and keyring secret
/// ([`ConnectorKind::secret_kind`]), then opens a live IMAP session.
///
/// Holds only a path, not a `Core` handle: every [`Self::connect`] call
/// opens its **own** fresh [`Core::open`] on that path to hand to
/// [`ImapMailProvider`] as its `RemoteLocator` -- deliberately not the same
/// `Core` the worker uses to call `tick`. `ImapMailProvider` owns its
/// locator by value, and `Core::tick` takes `&mut self`, so one `Core`
/// cannot sit in both places at once; two handles open on the same
/// on-disk WAL file is the supported way around that (SQLite serializes
/// the actual disk access, `busy_timeout = 5000` absorbs the rest), and is
/// exactly what `crates/app/src/shell.rs`'s `open_core_handles` already
/// does for its `core`/`reader` split.
///
/// `secrets`/`oauth` (T-083, third review round) are the two things a
/// prior review round found `connect()` still hardcoded even after
/// `build_session` centralized the wrap-or-not decision: a mutation that
/// rewrote `connect`'s Gmail branch to skip `build_session` entirely (call
/// `connect_generic` directly, i.e. turn off T-083's self-heal for every
/// OAuth account) passed the whole suite, because nothing called
/// [`ProviderFactory::connect`] itself on a real [`ImapProviderFactory`] --
/// every existing test drove `build_session`/`connect_with_bounded_reauth`/
/// `connect_generic` directly instead. `secrets: Arc<dyn SecretStore>`
/// reuses the exact blanket impl T-088 added in `feathermail_security`
/// (`impl<T: SecretStore + ?Sized> SecretStore for Arc<T>`) for the same
/// reason that impl exists: one shared trait-object handle, real
/// [`LibsecretStore`] or a test's [`feathermail_security::MemorySecretStore`],
/// installed once and reused everywhere this factory needs a `&dyn
/// SecretStore` including inside `OauthReauth` (see [`Self::connect_oauth`]) --
/// not a second, freshly-dialed store per call the way `connect()` briefly
/// held two separate `LibsecretStore` values before this change (one for
/// the primary attempt, one built again inside `connect_oauth`). `oauth:
/// Box<dyn OauthClientSource>` is the same shape for the other hardcoded
/// dependency. Both are set once by [`Self::new`] (production) or
/// [`Self::for_test`] (`#[cfg(test)]` only) and never branch on `cfg!(test)`
/// inside [`Self::connect`] itself -- the review that asked for this seam
/// explicitly ruled that out; a test-only *constructor* is the sanctioned
/// difference.
pub struct ImapProviderFactory {
    db_path: PathBuf,
    secrets: Arc<dyn SecretStore>,
    oauth: Box<dyn OauthClientSource>,
}

impl ImapProviderFactory {
    /// Unchanged signature and behavior for every real caller
    /// (`crates/app/src/shell.rs`'s only call site is off-limits for this
    /// task and was never touched): still infallible, still just a path.
    /// `secrets`/`oauth` are wired to the real [`LibsecretStore`]/
    /// [`LiveOauthClientSource`] here instead of dialed lazily inside
    /// `connect()` -- [`LibsecretStore::connect()`]'s constructor probe
    /// (a fail-fast startup check, not cached state -- every real
    /// `get`/`put` call dials the keyring bus fresh through its own
    /// private `session()` helper regardless) is not run here, so a
    /// keyring that is unreachable at construction time is no longer
    /// caught a moment earlier than it otherwise would be; it still
    /// surfaces from the very same `connect()` call, on the first real
    /// `secrets.get(...)`, which is what the user-visible outcome
    /// (does `connect()` fail, with a keyring-shaped error) actually is.
    pub fn new(db_path: impl Into<PathBuf>) -> Self {
        Self {
            db_path: db_path.into(),
            secrets: Arc::new(LibsecretStore),
            oauth: Box::new(LiveOauthClientSource),
        }
    }

    /// Test-only constructor (T-083, third review round): the sanctioned
    /// way to build an [`ImapProviderFactory`] whose [`Self::connect`]
    /// never touches the real system keyring or a real OAuth token
    /// endpoint, without any `cfg!(test)` branch inside `connect` itself.
    /// `db_path` is still a real path -- `connect()` always opens its own
    /// fresh `Core::open` on it either way (see this struct's own doc
    /// comment), so a test hands this a real tempfile, same as every
    /// other test in this module that needs a `Core`.
    #[cfg(test)]
    fn for_test(
        db_path: impl Into<PathBuf>,
        secrets: Arc<dyn SecretStore>,
        oauth: Box<dyn OauthClientSource>,
    ) -> Self {
        Self {
            db_path: db_path.into(),
            secrets,
            oauth,
        }
    }

    /// Shared machinery for [`ConnectorKind::Gmail`]/[`ConnectorKind::Microsoft`]
    /// (T-083): builds a real [`OauthReauth`] around whichever `oauth`
    /// [`Self::connect`] already loaded from `self.oauth`, and hands it,
    /// along with `key`/`attempt` [`Self::connect`] already built, to
    /// [`build_session`] -- the single place (production *and* tests call
    /// it, see this module's `#[cfg(test)]` block) that decides whether an
    /// account kind gets wrapped in [`ReauthingProvider`] or not. This
    /// method itself makes no part of that decision; it only differs
    /// between Gmail and Microsoft in which `Box<dyn TokenRefresh>` it was
    /// handed.
    ///
    /// `OauthReauth`'s own `secrets` is `self.secrets.clone()` -- an `Arc`
    /// clone, so it is the exact same underlying store `build_session`'s
    /// `secrets` argument reads from just below, not a second, independent
    /// one. That sharing is load-bearing, not just tidy: `OauthReauth::
    /// reauthenticate` writes the freshly refreshed access token back
    /// through it, and a later `connect()` call (or, in a test, a direct
    /// `secrets.get(...)` assertion) must see that write through the same
    /// handle, which a `MemorySecretStore` -- unlike the real, stateless
    /// [`LibsecretStore`] -- actually depends on.
    fn connect_oauth(
        &self,
        account: &AccountId,
        kind: ConnectorKind,
        oauth: Box<dyn TokenRefresh>,
        key: &SecretKey,
        attempt: impl FnOnce(&str) -> Result<ImapMailProvider<Core>, ConnectError>,
    ) -> Result<Box<dyn MailSession>, ConnectError> {
        let reauth = OauthReauth::new(
            account.clone(),
            self.db_path.clone(),
            oauth,
            self.secrets.clone(),
        );
        build_session(kind, &self.secrets, key, attempt, reauth)
    }
}

impl ProviderFactory for ImapProviderFactory {
    fn connect(&mut self, account: &AccountId) -> Result<Box<dyn MailSession>, ConnectError> {
        let locator = Core::open(&self.db_path).map_err(core_err_to_connect)?;
        let saved = locator
            .account_connection(account)
            .map_err(core_err_to_connect)?;
        let kind = ConnectorKind::from_provider_str(&saved.provider);

        let key = SecretKey::new(account.as_str(), kind.secret_kind());
        let form = saved.form.clone();
        let smtp_form = saved.form.clone();
        // Identical for every `ConnectorKind` -- only `kind.imap_auth`
        // varies, and that is already parameterized -- so it is built once
        // here and handed, unchanged, to whichever branch below needs it.
        let attempt = move |secret: &str| {
            ImapSession::connect(&form, kind.imap_auth(secret.to_string()))
                .map(|session| ImapMailProvider::new(session, locator))
        };

        // This `match` is *not* the "does this kind get `ReauthingProvider`"
        // decision -- that decision lives entirely inside `build_session`,
        // called uniformly by all three arms below. What differs here is
        // only which concrete OAuth client gets loaded (via `self.oauth`,
        // itself injected -- see this struct's own doc comment) and
        // wrapped into an `OauthReauth` before handing off to it (Google
        // vs. Microsoft load different clients; a password account loads
        // none at all). See `build_session`'s own doc comment for why the
        // wrap-or-not choice was moved out of this method in the first
        // place.
        let inner = match kind {
            ConnectorKind::Generic => build_session(kind, &self.secrets, &key, attempt, NoReauth),
            ConnectorKind::Gmail => {
                let oauth = self.oauth.google()?;
                self.connect_oauth(account, kind, oauth, &key, attempt)
            }
            ConnectorKind::Microsoft => {
                let oauth = self.oauth.microsoft()?;
                self.connect_oauth(account, kind, oauth, &key, attempt)
            }
        }?;
        let outbox = Core::open(&self.db_path).map_err(core_err_to_connect)?;
        Ok(Box::new(SendingSession {
            inner,
            outbox,
            form: smtp_form,
            auth: match kind {
                ConnectorKind::Generic => SmtpAuth::Password,
                ConnectorKind::Gmail | ConnectorKind::Microsoft => SmtpAuth::Xoauth2,
            },
            secrets: Arc::clone(&self.secrets),
            secret_key: key,
        }))
    }
}

/// Stand-in [`Reauthenticate`] for [`ConnectorKind::Generic`] accounts
/// (T-083, second review round): [`build_session`] takes a `reauth: R`
/// parameter unconditionally so its `Generic` and `Gmail`/`Microsoft`
/// branches can share one signature and one call site shape, but a
/// password account has no refresh token for any `R` to use --
/// `build_session`'s `Generic` arm calls [`connect_generic`], which never
/// references `reauth` at all. If this type's `reauthenticate` were ever
/// actually invoked, that would itself be the bug (a password account got
/// a bogus "reauthentication" attempt with nothing behind it), so it
/// panics loudly instead of pretending to succeed -- the same
/// self-enforcing shape as this module's test-only `NeverReauth`.
struct NoReauth;

impl Reauthenticate<ImapMailProvider<Core>> for NoReauth {
    fn reauthenticate(&mut self) -> Result<ImapMailProvider<Core>, ApplyError> {
        panic!(
            "NoReauth::reauthenticate() was called -- a Generic (password) \
             account has no refresh token, so build_session's Generic arm \
             must never reach this; something routed it through the \
             bounded-reauth path instead"
        );
    }
}

/// The full "which construction strategy does this account kind get"
/// decision (T-083, second review round) -- not just the two strategies
/// underneath it ([`connect_with_bounded_reauth`] / [`connect_generic`])
/// but the choice *between* them, plus the final boxing into the exact
/// `Box<dyn MailSession>` [`crate::worker`]'s background loop calls
/// `apply()` on.
///
/// # Why this exists
///
/// Before this function existed, that choice lived inline in
/// [`ImapProviderFactory::connect`]'s own `match kind` -- untested,
/// because neither `connect_with_bounded_reauth` nor `connect_generic`
/// being well-tested on its own proves `connect` calls the *right* one for
/// a given [`ConnectorKind`]. A review of the first round of T-083 found
/// exactly that gap by mutation: swapping which strategy the
/// Gmail/Microsoft branch called (to `connect_generic`, which is
/// precisely "stop giving this account the self-heal T-083 exists to
/// give it") left every test from that round green, because none of them
/// exercised `connect`'s own dispatch -- only the two strategies it
/// dispatches to.
///
/// Pulling the dispatch out here, generic over the provider type `P` (not
/// hardcoded to [`ImapMailProvider<Core>`]) the same way
/// `connect_with_bounded_reauth`/`connect_generic` are already generic,
/// closes that gap: a test can call the *exact* function
/// [`ImapProviderFactory::connect`] calls and observe the *exact* kind of
/// object [`crate::worker`] gets back, by driving `apply()` on the
/// returned box itself -- not by asserting which inner function ran. See
/// `build_session_reauths_a_gmail_account_on_a_stale_token_and_apply_succeeds`
/// and `build_session_never_reauths_a_generic_password_account` below.
///
/// `P: MailSession` (not just `MailProvider`) because both arms box the
/// result into `Box<dyn MailSession>`; `ReauthingProvider<P, R>:
/// MailSession` is an extra, explicit where-bound rather than something
/// implied by `P: MailSession` alone, because that impl is deliberately
/// *not* generic over every `P` -- see `ReauthingProvider`'s own
/// `MailSession` impl in this file for why (the concrete, `Sized` session
/// type its three forwarding methods need can only ever be produced by an
/// impl that knows what `P` actually is).
fn build_session<S, P, R>(
    kind: ConnectorKind,
    secrets: &S,
    key: &SecretKey,
    attempt: impl FnOnce(&str) -> Result<P, ConnectError>,
    reauth: R,
) -> Result<Box<dyn MailSession>, ConnectError>
where
    S: SecretStore,
    P: MailSession + 'static,
    R: Reauthenticate<P> + 'static,
    ReauthingProvider<P, R>: MailSession,
{
    match kind {
        // No refresh token exists for a password account (T-083): there
        // is nothing `ReauthingProvider` could ever fix by retrying, so
        // this stays exactly the single-attempt shape it always was --
        // never wrapped, `reauth` never touched.
        ConnectorKind::Generic => {
            let provider = connect_generic(secrets, key, attempt)?;
            Ok(Box::new(provider))
        }
        // Gmail/Microsoft both carry a refresh token, so both get the
        // bounded, at-most-once reauth-and-retry every later `apply()`
        // call gets from `ReauthingProvider`.
        ConnectorKind::Gmail | ConnectorKind::Microsoft => {
            let provider = connect_with_bounded_reauth(secrets, key, attempt, reauth)?;
            Ok(Box::new(provider))
        }
    }
}

/// The decision at the heart of [`ImapProviderFactory::connect_oauth`]
/// (T-083), pulled out narrow and generic so it can be exercised by a test
/// without a real system keyring or a real TCP/IMAP connection:
///
/// - `secrets`/`access_key` stand in for the real keyring lookup -- any
///   [`SecretStore`] works, so a test hands it a
///   [`feathermail_security::MemorySecretStore`] instead of
///   [`LibsecretStore`].
/// - `attempt` stands in for the real `ImapSession::connect` call -- any
///   "dial once with this secret" closure works, so a test hands it a
///   counting fake that never opens a socket.
/// - `reauth` is the exact [`Reauthenticate`] value the caller also hands
///   to the returned [`ReauthingProvider`] for every later `apply` call,
///   so a test can assert its call count directly -- the same shape
///   `feathermail_core::provider`'s own `ScriptedReauth` uses to pin down
///   "at most once, never a loop".
///
/// Behavior: read the saved secret and call `attempt` with it once. If no
/// secret is saved at all, or `attempt` comes back `ConnectError::Auth`
/// (the token was rejected), call `reauth.reauthenticate()` -- at most
/// once, never retried again if that also fails -- and use whatever it
/// returns in place of a second `attempt` call. Any other `attempt`
/// failure (network, a local config/invalid error) is returned straight
/// through without ever touching `reauth`, because nothing about those is
/// a token problem `reauth` could fix.
fn connect_with_bounded_reauth<S, P, R>(
    secrets: &S,
    access_key: &SecretKey,
    attempt: impl FnOnce(&str) -> Result<P, ConnectError>,
    mut reauth: R,
) -> Result<ReauthingProvider<P, R>, ConnectError>
where
    S: SecretStore,
    R: Reauthenticate<P>,
{
    let secret = secrets.get(access_key).map_err(secret_err_to_connect)?;
    let first = match secret {
        Some(secret) => attempt(secret.expose()),
        None => Err(ConnectError::reauth("no credential saved for this account")),
    };
    let provider = match first {
        Ok(provider) => provider,
        Err(ConnectError::Auth { .. }) => reauth.reauthenticate().map_err(apply_err_to_connect)?,
        Err(other) => return Err(other),
    };
    Ok(ReauthingProvider::new(provider, reauth))
}

/// The `Generic` (password) half of [`ImapProviderFactory::connect`]
/// (T-083), pulled out the same narrow way as
/// [`connect_with_bounded_reauth`] so it is testable without a real
/// system keyring -- but, unlike that function, this one takes no
/// [`Reauthenticate`] parameter at all: a plain password account has no
/// refresh token, so there is nothing a reauth attempt could ever fix.
/// The missing `R` parameter is a compiler-enforced fact, not just a
/// runtime habit a test happens to observe -- there is no value this
/// function could even pass to a `Reauthenticate::reauthenticate()` call
/// if it wanted to. `attempt` runs at most once (`FnOnce`); whatever it
/// returns, success or failure, is reported straight through unchanged --
/// no retry, no wrapping.
fn connect_generic<S: SecretStore, P>(
    secrets: &S,
    access_key: &SecretKey,
    attempt: impl FnOnce(&str) -> Result<P, ConnectError>,
) -> Result<P, ConnectError> {
    let secret = secrets
        .get(access_key)
        .map_err(secret_err_to_connect)?
        .ok_or_else(|| ConnectError::reauth("no credential saved for this account"))?;
    attempt(secret.expose())
}

/// [`Reauthenticate::reauthenticate`]'s own doc comment already draws the
/// `Auth` (terminal, "sign in again") vs. `Network` (retryable, D32's
/// existing backoff) line for T-083; this just carries that same
/// distinction across into [`ConnectError`]'s vocabulary for
/// [`connect_with_bounded_reauth`]'s single `?`. The three remaining
/// [`ApplyError`] variants (`Conflict`/`NotFound`/`Unsupported`) are
/// queue-apply outcomes a `Reauthenticate` impl has no legitimate reason
/// to ever return; they fall back to `Invalid` rather than panicking, on
/// the same "never crash on an unexpected value" principle as
/// `ConnectorKind::from_provider_str`'s fallback to `Generic`.
fn apply_err_to_connect(err: ApplyError) -> ConnectError {
    match err {
        ApplyError::Auth => ConnectError::reauth("token refresh did not produce a working session"),
        ApplyError::Network => ConnectError::network("token refresh endpoint unreachable"),
        ApplyError::Conflict
        | ApplyError::NotFound
        | ApplyError::Unsupported
        | ApplyError::NotEmpty => ConnectError::invalid("reauthentication failed unexpectedly"),
    }
}

/// Local SQLite/profile failure while preparing to connect (opening the
/// second `Core` handle, or reading a saved account's row) has no better
/// home in [`ConnectError`]'s vocabulary (D9 keeps it wire-shaped, same
/// reasoning as `feathermail_core::locator`'s own `sql_err_apply`):
/// `Invalid` is the closest fit -- this is a local/config problem, not a
/// network timeout or a rejected credential.
fn core_err_to_connect(err: CoreError) -> ConnectError {
    ConnectError::invalid(err.message)
}

/// [`SecretError`]'s `Display` is already human-safe (D14: it names kinds
/// and backends, never a secret value -- see `feathermail_security`'s own
/// `delete_account_tries_every_kind_even_when_one_delete_fails` test), so
/// it can be forwarded as-is.
fn secret_err_to_connect(err: SecretError) -> ConnectError {
    ConnectError::invalid(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::cell::Cell;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{mpsc, Mutex};
    use std::thread;

    use feathermail_core::{
        DraftContent, MailSecurity, MailboxForm, OpKind, OpStatus, Operation, OperationId,
        TickOutcome,
    };
    use feathermail_providers::test_support::spawn_fake_imap_server;
    use feathermail_providers::{ImapAuth, OauthError, TokenSet};
    use feathermail_security::MemorySecretStore;

    /// Same shape as `feathermail_core::provider::reauth_tests`'s own `op`
    /// helper -- `FakeProvider::apply` below ignores every field, so only
    /// the type has to be valid, not any particular value in it.
    fn op(id: &str) -> Operation {
        Operation {
            id: OperationId(id.to_string()),
            account_id: AccountId("acct".into()),
            target_id: "thread-1".into(),
            kind: OpKind::MarkRead,
            payload: "{}".into(),
            payload_hash: "hash".into(),
            created_at: 0,
            retry_count: 0,
            next_attempt_at: None,
            status: OpStatus::Pending,
            undo_of: None,
        }
    }

    fn imap_form(port: u16) -> MailboxForm {
        MailboxForm {
            email: "you@example.com".into(),
            imap_host: "127.0.0.1".into(),
            imap_port: port,
            imap_security: MailSecurity::None,
            smtp_host: "127.0.0.1".into(),
            smtp_port: 0,
            smtp_security: MailSecurity::None,
        }
    }

    fn connect_to_fake(port: u16) -> ImapSession {
        // Same settle-then-connect shape as `apply.rs`'s own fake-server
        // tests: `spawn_fake_imap_server` has already bound its listener
        // by the time it returns, but giving its accept loop a moment to
        // actually start running keeps this from ever racing a cold
        // thread scheduler.
        thread::sleep(Duration::from_millis(30));
        ImapSession::connect(&imap_form(port), ImapAuth::Login("x".into())).unwrap()
    }

    #[test]
    fn current_local_draft_roundtrips_to_the_discovered_fake_imap_drafts_mailbox() {
        const DRAFTS_MAILBOX: &str = "Cloud/Drafts";
        let (port, state) = spawn_fake_imap_server(
            vec![("INBOX", Vec::new()), (DRAFTS_MAILBOX, Vec::new())],
            true,
        );
        state
            .lock()
            .unwrap()
            .tag_special_use(DRAFTS_MAILBOX, "\\Drafts");

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("mail.db");
        Core::open(&db_path).unwrap();
        seed_account_and_folder(&db_path, "acct", "acct:inbox");
        let account = AccountId("acct".into());
        let mut core = Core::open(&db_path).unwrap();

        // Folder identity comes from the real fake server's LIST response,
        // not a guessed "Drafts" mailbox name.
        let mut discovery = connect_to_fake(port);
        let listings = discovery.list_folders().unwrap();
        drop(discovery);
        core.sync_folders(&account, &discovered_folders(&listings))
            .unwrap();

        let draft = core
            .save_draft(
                &account,
                None,
                DraftContent {
                    from: "writer@example.test".into(),
                    subject: "Server copy".into(),
                    body: "Local draft remains the source of truth.".into(),
                    ..DraftContent::default()
                },
            )
            .unwrap();
        let inner = Box::new(ImapMailProvider::new(
            connect_to_fake(port),
            Core::open(&db_path).unwrap(),
        ));
        let mut session = SendingSession {
            inner,
            outbox: Core::open(&db_path).unwrap(),
            form: imap_form(port),
            auth: SmtpAuth::Password,
            secrets: Arc::new(MemorySecretStore::new()),
            secret_key: SecretKey::password(account.as_str()),
        };

        let outcome = core.tick_for_account(&account, &mut session).unwrap();
        assert!(
            matches!(outcome, TickOutcome::Acked(_)),
            "draft APPEND must ACK through the queue, got {outcome:?}"
        );

        let state = state.lock().unwrap();
        let server_draft = state
            .mailboxes
            .get(DRAFTS_MAILBOX)
            .expect("real LIST must resolve the server Drafts mailbox");
        assert_eq!(server_draft.len(), 1);
        assert_eq!(server_draft[0].flags, vec!["\\Draft"]);
        assert_eq!(
            core.get_draft(&account, &draft.id).unwrap().remote_uid,
            Some(server_draft[0].uid),
            "the APPENDUID response must update the local server locator"
        );
        assert_eq!(
            core.get_draft(&account, &draft.id).unwrap().body,
            "Local draft remains the source of truth.",
            "D29: server sync never deletes or replaces an unsent local draft"
        );
    }

    /// Records the raw message handed to IMAP APPEND and fails the first
    /// append deliberately. The failure happens after SMTP has accepted the
    /// message, which is the only state transition where retrying must start
    /// at Sent rather than deliver a second copy to the recipient.
    type SentAppends = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

    struct SentAppendRecorder {
        appends: SentAppends,
        fail_first_append: bool,
    }

    impl MailProvider for SentAppendRecorder {
        fn apply(&mut self, _op: &Operation) -> Result<(), ApplyError> {
            panic!("SendingSession must handle Send before forwarding to its inner provider")
        }
    }

    impl MailSession for SentAppendRecorder {
        fn append_sent(&mut self, mailbox: &str, message: &[u8]) -> Result<(), ApplyError> {
            self.appends
                .lock()
                .unwrap()
                .push((mailbox.to_string(), message.to_vec()));
            if std::mem::replace(&mut self.fail_first_append, false) {
                Err(ApplyError::Network)
            } else {
                Ok(())
            }
        }

        fn sync_one_folder(
            &mut self,
            _store: &mut CoreSyncStore<'_>,
            _folder: &str,
            _now: i64,
            _is_cancelled: &dyn Fn() -> bool,
        ) -> Result<SyncOutcome, SyncError> {
            panic!("outbox delivery never performs an inbound folder sync")
        }

        fn open_one_body(
            &mut self,
            _core: &mut Core,
            _id: &MessageId,
            _bodies_dir: &Path,
        ) -> Result<Vec<u8>, CoreError> {
            panic!("outbox delivery never opens an incoming body")
        }

        fn idle_once(
            &mut self,
            _folder: &str,
            _idle_timeout: Duration,
            _should_stop: &mut dyn FnMut() -> bool,
        ) -> Result<IdleRound, ConnectError> {
            panic!("outbox delivery never enters IMAP IDLE")
        }
    }

    /// Small real SMTP peer for the T-045 service-level delivery test. It
    /// accepts exactly one authenticated DATA transaction and exposes only
    /// the resulting RFC 5322 bytes to the test; credentials stay local to
    /// the socket exchange and are never logged or sent through its channel.
    fn capture_one_smtp_message() -> (u16, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut writer = stream.try_clone().unwrap();
            writer.write_all(b"220 test smtp\r\n").unwrap();
            let mut reader = BufReader::new(stream);
            let mut data = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                if line.starts_with("EHLO") {
                    writer
                        .write_all(b"250-test\r\n250 AUTH PLAIN LOGIN\r\n")
                        .unwrap();
                } else if line.starts_with("AUTH") {
                    writer.write_all(b"235 authenticated\r\n").unwrap();
                } else if line.starts_with("MAIL FROM") || line.starts_with("RCPT TO") {
                    writer.write_all(b"250 ok\r\n").unwrap();
                } else if line.starts_with("DATA") {
                    writer.write_all(b"354 send it\r\n").unwrap();
                    loop {
                        let mut body_line = String::new();
                        reader.read_line(&mut body_line).unwrap();
                        if body_line == ".\r\n" {
                            break;
                        }
                        data.push_str(&body_line);
                    }
                    writer.write_all(b"250 queued\r\n").unwrap();
                    tx.send(std::mem::take(&mut data)).unwrap();
                } else if line.starts_with("QUIT") {
                    writer.write_all(b"221 bye\r\n").unwrap();
                    break;
                } else {
                    writer.write_all(b"250 ok\r\n").unwrap();
                }
            }
        });
        (port, rx)
    }

    #[test]
    fn sent_append_retry_never_resends_a_message_smtp_already_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("mail.db");
        let account = AccountId("acct".into());
        let mut core = Core::open(&db_path).unwrap();
        seed_account_and_folder(&db_path, account.as_str(), "acct:inbox");
        rusqlite::Connection::open(&db_path)
            .unwrap()
            .execute(
                "INSERT INTO folders (id, account_id, remote_id, name, kind) \
                 VALUES ('acct:sent', 'acct', 'Sent', 'Sent', 'sent')",
                [],
            )
            .unwrap();
        core.set_now(0);
        let draft = core
            .save_draft(
                &account,
                None,
                DraftContent {
                    from: "writer@example.test".into(),
                    to: "recipient@example.test".into(),
                    subject: "Durable delivery".into(),
                    body: "Private message body.".into(),
                    ..DraftContent::default()
                },
            )
            .unwrap();
        // This test owns SMTP, not T-042's IMAP Drafts synchronisation;
        // remove the unrelated revision operation so Send is the next row
        // claimed through the real Core queue.
        rusqlite::Connection::open(&db_path)
            .unwrap()
            .execute(
                "UPDATE operations SET status = 'cancelled' \
                 WHERE account_id = 'acct' AND op = 'sync_draft'",
                [],
            )
            .unwrap();
        let operation = core.queue_draft_send(&account, &draft.id).unwrap();
        let outbox_id: String = rusqlite::Connection::open(&db_path)
            .unwrap()
            .query_row(
                "SELECT target_id FROM operations WHERE id = ?1",
                [operation.as_str()],
                |row| row.get(0),
            )
            .unwrap();

        let (smtp_port, smtp_messages) = capture_one_smtp_message();
        let secrets = Arc::new(MemorySecretStore::new());
        secrets
            .put(&SecretKey::password(account.as_str()), "test-password")
            .unwrap();
        let appends = Arc::new(Mutex::new(Vec::new()));
        let mut session = SendingSession {
            inner: Box::new(SentAppendRecorder {
                appends: Arc::clone(&appends),
                fail_first_append: true,
            }),
            outbox: Core::open(&db_path).unwrap(),
            form: MailboxForm {
                email: "writer@example.test".into(),
                imap_host: "127.0.0.1".into(),
                imap_port: 0,
                imap_security: MailSecurity::None,
                smtp_host: "127.0.0.1".into(),
                smtp_port,
                smtp_security: MailSecurity::None,
            },
            auth: SmtpAuth::Password,
            secrets,
            secret_key: SecretKey::password(account.as_str()),
        };

        assert!(matches!(
            core.tick_for_account(&account, &mut session).unwrap(),
            TickOutcome::Retry { id, delay: 2 } if id == operation
        ));
        let smtp = smtp_messages
            .recv_timeout(Duration::from_secs(2))
            .expect("SMTP must receive the queued message before Sent APPEND can fail");
        assert!(smtp.contains("Subject: Durable delivery"));
        assert!(smtp.contains("Private message body."));
        assert!(!smtp.contains("test-password"));
        assert_eq!(
            core.load_outbox(&account, &outbox_id).unwrap().status,
            "delivered"
        );

        core.set_now(2);
        assert!(matches!(
            core.tick_for_account(&account, &mut session).unwrap(),
            TickOutcome::Acked(id) if id == operation
        ));
        assert_eq!(
            core.load_outbox(&account, &outbox_id).unwrap().status,
            "sent"
        );
        let appends = appends.lock().unwrap();
        assert_eq!(
            appends.len(),
            2,
            "Sent APPEND must retry once after its network failure"
        );
        assert!(appends.iter().all(|(mailbox, _)| mailbox == "Sent"));
        assert!(
            appends
                .iter()
                .all(|(_, message)| String::from_utf8_lossy(message)
                    .contains("Private message body."))
        );
    }

    /// A real SMTP peer that hangs up in the middle of the first message's
    /// `DATA` and serves the retry in full (T-112). Two sequential
    /// connections, because `send_message` dials a fresh one per attempt.
    /// The channel carries only *completed* deliveries -- a transaction the
    /// server never acknowledged is not one -- so a test can count how many
    /// copies the recipient would actually have received.
    fn smtp_that_drops_the_first_data() -> (u16, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut drop_this_one = true;
            while let Ok((stream, _)) = listener.accept() {
                let mut writer = stream.try_clone().unwrap();
                writer.write_all(b"220 test smtp\r\n").unwrap();
                let mut reader = BufReader::new(stream);
                let mut data = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        break;
                    }
                    if line.starts_with("EHLO") {
                        let _ = writer.write_all(b"250-test\r\n250 AUTH PLAIN LOGIN\r\n");
                    } else if line.starts_with("AUTH") {
                        let _ = writer.write_all(b"235 authenticated\r\n");
                    } else if line.starts_with("MAIL FROM") || line.starts_with("RCPT TO") {
                        let _ = writer.write_all(b"250 ok\r\n");
                    } else if line.starts_with("DATA") {
                        let _ = writer.write_all(b"354 send it\r\n");
                        if std::mem::replace(&mut drop_this_one, false) {
                            // Read one line of the message, then vanish: the
                            // server never saw the terminating dot, so it
                            // never queued anything.
                            let mut body_line = String::new();
                            let _ = reader.read_line(&mut body_line);
                            break;
                        }
                        loop {
                            let mut body_line = String::new();
                            if reader.read_line(&mut body_line).unwrap_or(0) == 0 {
                                break;
                            }
                            if body_line == ".\r\n" {
                                break;
                            }
                            data.push_str(&body_line);
                        }
                        let _ = writer.write_all(b"250 queued\r\n");
                        tx.send(std::mem::take(&mut data)).unwrap();
                    } else if line.starts_with("QUIT") {
                        let _ = writer.write_all(b"221 bye\r\n");
                        break;
                    } else {
                        let _ = writer.write_all(b"250 ok\r\n");
                    }
                }
            }
        });
        (port, rx)
    }

    /// T-070's "unstable network ... no duplicate send", the half that can be
    /// settled without a stand: the connection dies *before* the server ever
    /// acknowledged the message. The queue must retry, the recipient must end
    /// up with exactly one copy, and the outbox row must not claim delivery
    /// for a transaction no server ever completed.
    ///
    /// The other half -- the server's `250` is lost on the way back, so it
    /// queued a message the client believes failed -- is not decidable by any
    /// SMTP client and is not claimed here; see `docs/plan.md` T-112.
    #[test]
    fn smtp_cut_off_mid_data_retries_and_delivers_exactly_one_copy() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("mail.db");
        let account = AccountId("acct".into());
        let mut core = Core::open(&db_path).unwrap();
        seed_account_and_folder(&db_path, account.as_str(), "acct:inbox");
        rusqlite::Connection::open(&db_path)
            .unwrap()
            .execute(
                "INSERT INTO folders (id, account_id, remote_id, name, kind) \
                 VALUES ('acct:sent', 'acct', 'Sent', 'Sent', 'sent')",
                [],
            )
            .unwrap();
        core.set_now(0);
        let draft = core
            .save_draft(
                &account,
                None,
                DraftContent {
                    from: "writer@example.test".into(),
                    to: "recipient@example.test".into(),
                    subject: "Only once".into(),
                    body: "Private message body.".into(),
                    ..DraftContent::default()
                },
            )
            .unwrap();
        rusqlite::Connection::open(&db_path)
            .unwrap()
            .execute(
                "UPDATE operations SET status = 'cancelled' \
                 WHERE account_id = 'acct' AND op = 'sync_draft'",
                [],
            )
            .unwrap();
        let operation = core.queue_draft_send(&account, &draft.id).unwrap();
        let outbox_id: String = rusqlite::Connection::open(&db_path)
            .unwrap()
            .query_row(
                "SELECT target_id FROM operations WHERE id = ?1",
                [operation.as_str()],
                |row| row.get(0),
            )
            .unwrap();

        let (smtp_port, delivered) = smtp_that_drops_the_first_data();
        let secrets = Arc::new(MemorySecretStore::new());
        secrets
            .put(&SecretKey::password(account.as_str()), "test-password")
            .unwrap();
        let appends = Arc::new(Mutex::new(Vec::new()));
        let mut session = SendingSession {
            inner: Box::new(SentAppendRecorder {
                appends: Arc::clone(&appends),
                fail_first_append: false,
            }),
            outbox: Core::open(&db_path).unwrap(),
            form: MailboxForm {
                email: "writer@example.test".into(),
                imap_host: "127.0.0.1".into(),
                imap_port: 0,
                imap_security: MailSecurity::None,
                smtp_host: "127.0.0.1".into(),
                smtp_port,
                smtp_security: MailSecurity::None,
            },
            auth: SmtpAuth::Password,
            secrets,
            secret_key: SecretKey::password(account.as_str()),
        };

        assert!(matches!(
            core.tick_for_account(&account, &mut session).unwrap(),
            TickOutcome::Retry { id, delay: 2 } if id == operation
        ));
        assert_eq!(
            core.load_outbox(&account, &outbox_id).unwrap().status,
            "queued",
            "a transaction the server never acknowledged must not be recorded as delivered"
        );
        assert!(
            appends.lock().unwrap().is_empty(),
            "nothing may reach Sent while the message itself has not left"
        );

        core.set_now(2);
        assert!(matches!(
            core.tick_for_account(&account, &mut session).unwrap(),
            TickOutcome::Acked(id) if id == operation
        ));
        assert_eq!(
            core.load_outbox(&account, &outbox_id).unwrap().status,
            "sent"
        );

        let first = delivered
            .recv_timeout(Duration::from_secs(5))
            .expect("the retry must actually deliver the message");
        assert!(first.contains("Subject: Only once"));
        assert!(first.contains("Private message body."));
        assert!(!first.contains("test-password"));
        assert!(
            delivered.recv_timeout(Duration::from_millis(300)).is_err(),
            "the recipient must not receive a second copy of the same message"
        );
        assert_eq!(appends.lock().unwrap().len(), 1);
    }

    /// A [`Reauthenticate`] double that must never be called. Used by the
    /// three forwarding tests below, none of which ever produce an `Auth`
    /// failure -- if any of them somehow did reach a real reauth attempt,
    /// that would be a second bug hiding behind the one the test is
    /// actually about, so this fails loudly (a panic, not a wrong answer)
    /// instead of silently doing something plausible.
    struct NeverReauth;

    impl Reauthenticate<ImapMailProvider<Core>> for NeverReauth {
        fn reauthenticate(&mut self) -> Result<ImapMailProvider<Core>, ApplyError> {
            panic!(
                "reauthenticate() was called, but nothing in this test ever \
                 produces an Auth failure -- this is a second bug, not the \
                 one this test is checking for"
            );
        }
    }

    // --- Test 1: MailSession forwarding actually reaches the inner session ---
    //
    // Each sub-test below drives one of `ReauthingProvider`'s three
    // `MailSession` methods against a real fake IMAP server
    // (`feathermail_providers::test_support`) and then inspects the fake
    // server's own mutated state -- not the call's return value -- as
    // proof the wrapper did not just stub the call out. `NeverReauth`
    // guarantees the reauth path (tested separately below) never gets
    // credit for work these tests didn't ask it to do.

    #[test]
    fn reauthing_provider_sync_one_folder_reaches_the_real_session_not_a_stub() {
        let (port, state) = spawn_fake_imap_server(vec![], true);
        let session = connect_to_fake(port);
        let provider = ImapMailProvider::new(session, Core::memory().unwrap());
        let mut wrapped = ReauthingProvider::new(provider, NeverReauth);

        // A separate, on-disk `Core` just to host the sync-progress store --
        // see `CoreSyncStore`'s own doc comment: it is bound to an
        // account/folder-id pair for storage identity only, unrelated to
        // the `folder` (IMAP mailbox name) argument below. `sync_state`
        // carries real foreign keys into `accounts`/`folders` (unlike the
        // fixture text quoted in some older migration tests), so both rows
        // have to actually exist first -- `Core::memory()` can't be seeded
        // by a second connection, hence the tempfile.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("core.sqlite3");
        {
            Core::open(&db_path).unwrap();
        } // creates the schema
        seed_account_and_folder(&db_path, "acct", "folder-row");
        let store_core = Core::open(&db_path).unwrap();
        let account = AccountId("acct".into());
        let mut store = store_core.sync_store(&account, "folder-row");

        wrapped
            .sync_one_folder(&mut store, "Never/Seen/Before", 0, &|| false)
            .expect(
                "a never-before-seen, empty mailbox must sync cleanly \
                 through the wrapper (uidnext <= 1 short-circuits before \
                 any FETCH is needed)",
            );

        assert!(
            state
                .lock()
                .unwrap()
                .mailboxes
                .contains_key("Never/Seen/Before"),
            "the fake server never saw a SELECT for this mailbox -- \
             ReauthingProvider::sync_one_folder did not reach the real \
             session, it only looked like it did"
        );
    }

    #[test]
    fn reauthing_provider_open_one_body_reaches_the_real_session_not_a_stub() {
        let (port, state) = spawn_fake_imap_server(vec![], true);
        let session = connect_to_fake(port);

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("core.sqlite3");
        {
            Core::open(&db_path).unwrap();
        } // creates the schema
        seed_message_with_remote_folder(&db_path, "msg-1", "Never/Fetched/Folder", 42);

        let provider_core = Core::open(&db_path).unwrap();
        let provider = ImapMailProvider::new(session, provider_core);
        let mut wrapped = ReauthingProvider::new(provider, NeverReauth);

        let mut open_core = Core::open(&db_path).unwrap();
        let bodies_dir = dir.path().join("bodies");
        std::fs::create_dir_all(&bodies_dir).unwrap();

        // `test_support`'s fake has no FETCH handler at all, so this call
        // is expected to come back `Err` -- what this test actually
        // checks is that a real `SELECT` for the message's folder went
        // out on the wire first (`feathermail_sync::fetch_body` always
        // selects before fetching), proving the call reached the real
        // session rather than a stub that never touched the network.
        let _ = wrapped.open_one_body(&mut open_core, &MessageId("msg-1".into()), &bodies_dir);

        assert!(
            state
                .lock()
                .unwrap()
                .mailboxes
                .contains_key("Never/Fetched/Folder"),
            "the fake server never saw a SELECT for this folder -- \
             ReauthingProvider::open_one_body did not reach the real \
             session, it only looked like it did"
        );
    }

    #[test]
    fn reauthing_provider_idle_once_reaches_the_real_session_not_a_stub() {
        let (port, state) = spawn_fake_imap_server(vec![], true);
        let session = connect_to_fake(port);
        let provider = ImapMailProvider::new(session, Core::memory().unwrap());
        let mut wrapped = ReauthingProvider::new(provider, NeverReauth);

        // `test_support`'s fake never advertises IDLE in its CAPABILITY
        // response, and `should_stop` fires on the very first check, so
        // `run_idle_with` takes its fastest, deterministic path: a plain
        // `SELECT` + `CAPABILITY` round trip and an immediate `Stopped`,
        // no `NOOP`/`IDLE` support required from the fake at all.
        let round = wrapped
            .idle_once("Watched/Folder", Duration::from_secs(5), &mut || true)
            .expect("idle_once must reach the real session, not a stub");

        assert!(!round.idle_capable);
        assert!(matches!(round.outcome, IdleOutcome::Stopped));
        assert!(
            state
                .lock()
                .unwrap()
                .mailboxes
                .contains_key("Watched/Folder"),
            "the fake server never saw a SELECT for this folder -- \
             ReauthingProvider::idle_once did not reach the real session, \
             it only looked like it did"
        );
    }

    /// Seeds just enough of `accounts`/`folders` for one `(account_id,
    /// folder_id)` pair to satisfy `sync_state`'s foreign keys, so
    /// `CoreSyncStore::save_state`/`load_state` can actually write rather
    /// than fail on a constraint the seeded-account tests never hit
    /// otherwise (this test is the only one in this module driving a real
    /// sync pass against an on-disk profile).
    fn seed_account_and_folder(db_path: &std::path::Path, account_id: &str, folder_id: &str) {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO accounts (id, name, email, provider, status, download_policy, created_at, updated_at)
             VALUES (?1, ?1, 'a@example.com', 'generic', 'synced', 'recent', 0, 0)",
            rusqlite::params![account_id],
        )
        .unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO folders (id, account_id, name, kind) VALUES (?1, ?2, 'Inbox', 'inbox')",
            rusqlite::params![folder_id, account_id],
        )
        .unwrap();
    }

    /// Seeds just enough of `accounts`/`folders` (with a real `remote_id`)/
    /// `threads`/`messages` (with a real `provider_uid`) for
    /// [`Core::open_body`] to resolve a message down to a live `SELECT` +
    /// `FETCH` attempt, rather than stopping early at one of its own
    /// "not synced yet" guards. Mirrors `crates/core/src/body.rs`'s own
    /// `seed_message` test helper (that module is off-limits for this
    /// task), extended with the two columns `seed_message` leaves NULL
    /// on purpose for its own (different) test cases.
    fn seed_message_with_remote_folder(
        db_path: &std::path::Path,
        msg_id: &str,
        remote_folder: &str,
        uid: i64,
    ) {
        let acc = "acct";
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO accounts (id, name, email, provider, status, download_policy, created_at, updated_at)
             VALUES (?1, ?1, 'a@example.com', 'generic', 'synced', 'recent', 0, 0)",
            rusqlite::params![acc],
        )
        .unwrap();
        let folder = format!("{acc}:inbox");
        conn.execute(
            "INSERT OR IGNORE INTO folders (id, account_id, remote_id, name, kind) \
             VALUES (?1, ?2, ?3, 'Inbox', 'inbox')",
            rusqlite::params![folder, acc, remote_folder],
        )
        .unwrap();
        let thread = format!("{acc}:t:{msg_id}");
        conn.execute(
            "INSERT OR IGNORE INTO threads (id, account_id, folder_id, subject, snippet, date, unread) \
             VALUES (?1, ?2, ?3, 'Hello', 'Hi', 0, 0)",
            rusqlite::params![thread, acc, folder],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (id, account_id, thread_id, folder_id, provider_uid, date) \
             VALUES (?1, ?2, ?3, ?4, ?5, 0)",
            rusqlite::params![msg_id, acc, thread, folder, uid],
        )
        .unwrap();
    }

    // --- Tests 2 & 3: `connect_with_bounded_reauth`'s at-most-once bound ---

    /// Scripted [`Reauthenticate`] double for `connect_with_bounded_reauth`
    /// tests: records every call in a shared `Rc<Cell<u32>>` (so the count
    /// is still readable after the double itself is moved into the
    /// `ReauthingProvider` the seam returns -- `ReauthingProvider`'s
    /// `reauth` field is private to `feathermail_core::provider`, not
    /// reachable from here) and refuses a second call itself, the same
    /// self-enforcing style as `feathermail_core::provider`'s own
    /// `ScriptedReauth`: a regression that turns the bounded retry back
    /// into a loop must fail the test by name, not hang `cargo test`.
    struct CountingReauth<P> {
        calls: Rc<Cell<u32>>,
        outcome: fn() -> Result<P, ApplyError>,
    }

    impl<P> Reauthenticate<P> for CountingReauth<P> {
        fn reauthenticate(&mut self) -> Result<P, ApplyError> {
            let n = self.calls.get() + 1;
            self.calls.set(n);
            assert!(
                n <= 1,
                "reauthenticate() called {n} times by connect_with_bounded_reauth \
                 -- the at-most-once bound is gone, and a real token endpoint \
                 would be hammered until it bans us"
            );
            (self.outcome)()
        }
    }

    #[test]
    fn oauth_connect_refreshes_the_token_exactly_once_after_a_rejection_then_succeeds() {
        let secrets = MemorySecretStore::new();
        let key = SecretKey::oauth_access("acct");
        secrets.put(&key, "stale-token").unwrap();

        let calls = Rc::new(Cell::new(0u32));
        let reauth = CountingReauth {
            calls: calls.clone(),
            outcome: || Ok(42u32),
        };

        let result = connect_with_bounded_reauth(
            &secrets,
            &key,
            |secret| {
                assert_eq!(secret, "stale-token");
                Err(ConnectError::auth("token rejected"))
            },
            reauth,
        );

        let mut provider = result
            .expect("a primary rejection followed by a successful refresh must yield a session");
        assert_eq!(*provider.inner_mut(), 42);
        assert_eq!(
            calls.get(),
            1,
            "a primary connect rejected for auth must trigger exactly one \
             refresh attempt, not zero and not more than one"
        );
    }

    #[test]
    fn oauth_connect_reports_need_reauth_when_the_refresh_also_fails_without_looping() {
        let secrets = MemorySecretStore::new();
        let key = SecretKey::oauth_access("acct");
        secrets.put(&key, "stale-token").unwrap();

        let calls = Rc::new(Cell::new(0u32));
        let reauth = CountingReauth::<u32> {
            calls: calls.clone(),
            outcome: || Err(ApplyError::Auth),
        };

        let result = connect_with_bounded_reauth(
            &secrets,
            &key,
            |_secret| Err(ConnectError::auth("token rejected")),
            reauth,
        );

        assert!(
            matches!(result, Err(ConnectError::Auth { .. })),
            "a refresh that also fails must surface as a plain \"need \
             reauth\" error to the caller, not succeed and not hang"
        );
        assert_eq!(
            calls.get(),
            1,
            "a refresh that fails must not be retried in a loop -- exactly \
             one attempt, then give up"
        );
    }

    // --- Test 4: a Generic (password) account is never wrapped, never refreshed ---

    #[test]
    fn generic_account_password_rejection_is_reported_once_never_retried_as_a_refresh() {
        let secrets = MemorySecretStore::new();
        let key = SecretKey::password("acct");
        secrets.put(&key, "hunter2").unwrap();

        let attempts = Rc::new(Cell::new(0u32));
        let attempts_inner = attempts.clone();
        // `connect_generic`'s signature has no `Reauthenticate` parameter
        // at all -- unlike `connect_with_bounded_reauth`, there is no
        // value it could even pass to a `reauthenticate()` call if it
        // wanted to, so "zero refresh attempts" is a fact the type system
        // enforces here, not just a runtime habit. What this test pins
        // down at runtime is the other half: `attempt` itself is not
        // retried either -- a rejected password is reported once, as-is.
        let result: Result<u32, ConnectError> = connect_generic(&secrets, &key, move |secret| {
            let n = attempts_inner.get() + 1;
            attempts_inner.set(n);
            assert!(
                n <= 1,
                "connect_generic dialed `attempt` {n} times -- a Generic \
                     account has no refresh token to retry with, so this \
                     must never happen more than once"
            );
            assert_eq!(secret, "hunter2");
            Err(ConnectError::auth("bad password"))
        });

        assert!(
            matches!(result, Err(ConnectError::Auth { .. })),
            "a rejected password must be reported as-is, not silently \
             retried or wrapped away"
        );
        assert_eq!(
            attempts.get(),
            1,
            "connect_generic must attempt the connection exactly once"
        );
    }

    // --- Test 5 (review round 2): `build_session`'s dispatch is itself
    // tested, not just the two strategies it dispatches to ---
    //
    // A prior review round found that swapping `connect_with_bounded_reauth`
    // for `connect_generic` inside the Gmail/Microsoft branch -- i.e.
    // silently turning off T-083's self-heal for every OAuth account --
    // left every test above green, because none of them drove the
    // `match kind` that picks between the two. `build_session` (this
    // file, above) is now the one place that decision lives, called by
    // both `ImapProviderFactory::connect` and the tests below. `FakeProvider`
    // stands in for a real `ImapMailProvider<Core>` here on purpose: no
    // `ImapSession` command ever actually produces `ConnectError::Auth`
    // mid-session (only `LOGIN`/`XOAUTH2` can, and `Core`'s own
    // `RemoteLocator` impl never returns `ApplyError::Auth` either) -- a
    // live fake IMAP server has no way to fabricate the "access token went
    // stale under an already-open session" failure `ReauthingProvider` is
    // built to catch, so exercising that branch honestly needs a provider
    // double that can simulate it directly, the same substitution
    // `connect_with_bounded_reauth`'s own tests already make for `attempt`.

    /// A [`MailProvider`] double that fails `apply()` with
    /// [`ApplyError::Auth`] for its first `fail_first_n` calls, then always
    /// succeeds -- the same "one stale-token failure, then a working
    /// retry" shape a real expired access token would produce, without a
    /// live IMAP server in the loop.
    struct FakeProvider {
        fail_first_n: u32,
        calls: u32,
    }

    impl MailProvider for FakeProvider {
        fn apply(&mut self, _op: &Operation) -> Result<(), ApplyError> {
            self.calls += 1;
            if self.calls <= self.fail_first_n {
                Err(ApplyError::Auth)
            } else {
                Ok(())
            }
        }
    }

    // The three `MailSession` forwarding methods are never called by the
    // dispatch tests below (they only ever call `apply()`, `MailProvider`'s
    // method) -- these panic rather than fabricate a plausible-looking
    // result if that ever changes, the same "fail loud, not quiet" shape
    // as this module's `NeverReauth`.
    impl MailSession for FakeProvider {
        fn sync_one_folder(
            &mut self,
            _store: &mut CoreSyncStore<'_>,
            _folder: &str,
            _now: i64,
            _is_cancelled: &dyn Fn() -> bool,
        ) -> Result<SyncOutcome, SyncError> {
            unimplemented!("build_session's dispatch tests never call sync_one_folder")
        }

        fn open_one_body(
            &mut self,
            _core: &mut Core,
            _id: &MessageId,
            _bodies_dir: &Path,
        ) -> Result<Vec<u8>, CoreError> {
            unimplemented!("build_session's dispatch tests never call open_one_body")
        }

        fn idle_once(
            &mut self,
            _folder: &str,
            _idle_timeout: Duration,
            _should_stop: &mut dyn FnMut() -> bool,
        ) -> Result<IdleRound, ConnectError> {
            unimplemented!("build_session's dispatch tests never call idle_once")
        }
    }

    // `build_session`'s `where ReauthingProvider<P, R>: MailSession` bound
    // (needed for its Gmail/Microsoft arm to box the result) has to be
    // satisfiable for `P = FakeProvider` too -- this is that impl, the
    // test-only mirror of the real one above for `ImapMailProvider<Core>`.
    impl<R: Reauthenticate<FakeProvider>> MailSession for ReauthingProvider<FakeProvider, R> {
        fn sync_one_folder(
            &mut self,
            store: &mut CoreSyncStore<'_>,
            folder: &str,
            now: i64,
            is_cancelled: &dyn Fn() -> bool,
        ) -> Result<SyncOutcome, SyncError> {
            self.inner_mut()
                .sync_one_folder(store, folder, now, is_cancelled)
        }

        fn open_one_body(
            &mut self,
            core: &mut Core,
            id: &MessageId,
            bodies_dir: &Path,
        ) -> Result<Vec<u8>, CoreError> {
            self.inner_mut().open_one_body(core, id, bodies_dir)
        }

        fn idle_once(
            &mut self,
            folder: &str,
            idle_timeout: Duration,
            should_stop: &mut dyn FnMut() -> bool,
        ) -> Result<IdleRound, ConnectError> {
            self.inner_mut()
                .idle_once(folder, idle_timeout, should_stop)
        }
    }

    /// Scripted [`Reauthenticate<FakeProvider>`] double: refuses a second
    /// call itself (same self-enforcing shape as `CountingReauth` above),
    /// and always hands back a `FakeProvider` that no longer fails.
    struct FakeReauth {
        calls: Rc<Cell<u32>>,
    }

    impl Reauthenticate<FakeProvider> for FakeReauth {
        fn reauthenticate(&mut self) -> Result<FakeProvider, ApplyError> {
            let n = self.calls.get() + 1;
            self.calls.set(n);
            assert!(
                n <= 1,
                "build_session's Gmail/Microsoft branch called \
                 reauthenticate() {n} times for one apply() call -- the \
                 bounded, at-most-once retry is gone"
            );
            Ok(FakeProvider {
                fail_first_n: 0,
                calls: 0,
            })
        }
    }

    fn assert_kind_gets_bounded_reauth(kind: ConnectorKind) {
        let secrets = MemorySecretStore::new();
        let key = SecretKey::oauth_access("acct");
        secrets.put(&key, "token").unwrap();

        let reauth_calls = Rc::new(Cell::new(0u32));
        let reauth = FakeReauth {
            calls: reauth_calls.clone(),
        };

        let mut session = build_session(
            kind,
            &secrets,
            &key,
            |_secret| {
                Ok(FakeProvider {
                    fail_first_n: 1,
                    calls: 0,
                })
            },
            reauth,
        )
        .expect("the primary attempt here always succeeds, so connect must too");

        session.apply(&op("op-1")).expect(
            "apply() must self-heal a stale token: the first inner apply() \
             fails Auth, reauthenticate() runs exactly once, and the retry \
             against the fresh provider must succeed",
        );
        assert_eq!(
            reauth_calls.get(),
            1,
            "reauthenticate() must have run exactly once for this account \
             kind -- if this is 0, build_session stopped wrapping this \
             kind in ReauthingProvider, which is exactly the regression a \
             prior review round found slipping through every other test \
             undetected"
        );
    }

    #[test]
    fn build_session_reauths_a_gmail_account_on_a_stale_token_and_apply_succeeds() {
        assert_kind_gets_bounded_reauth(ConnectorKind::Gmail);
    }

    #[test]
    fn build_session_reauths_a_microsoft_account_on_a_stale_token_and_apply_succeeds() {
        assert_kind_gets_bounded_reauth(ConnectorKind::Microsoft);
    }

    #[test]
    fn build_session_never_reauths_a_generic_password_account() {
        let secrets = MemorySecretStore::new();
        let key = SecretKey::password("acct");
        secrets.put(&key, "hunter2").unwrap();

        let reauth_calls = Rc::new(Cell::new(0u32));
        let reauth = FakeReauth {
            calls: reauth_calls.clone(),
        };

        let mut session = build_session(
            ConnectorKind::Generic,
            &secrets,
            &key,
            |_secret| {
                Ok(FakeProvider {
                    fail_first_n: 1,
                    calls: 0,
                })
            },
            reauth,
        )
        .expect("the primary attempt here always succeeds, so connect must too");

        let result = session.apply(&op("op-1"));
        assert!(
            matches!(result, Err(ApplyError::Auth)),
            "a Generic account's session must never be wrapped in \
             ReauthingProvider -- an Auth failure from apply() must come \
             back exactly as the inner provider reported it, not get \
             silently retried"
        );
        assert_eq!(
            reauth_calls.get(),
            0,
            "a Generic (password) account has no refresh token -- \
             reauthenticate() must never be called for it, no matter what \
             apply() returns"
        );
    }

    // --- Test 6 (review round 3): `ImapProviderFactory::connect` itself,
    // end to end -- not `build_session`, not `connect_with_bounded_reauth`
    // in isolation, but the exact `fn connect` `crate::worker`'s
    // background loop calls, on a real `ImapProviderFactory`.
    //
    // A prior review round found this exact gap by mutation: rewriting
    // `connect`'s Gmail branch to skip `build_session` entirely (call
    // `connect_generic` directly -- i.e. turn T-083's self-heal off for
    // every OAuth account) left every test above green, `cargo test -p
    // feathermail-service` reporting 37 passed / 0 failed, because none of
    // them ever called `ProviderFactory::connect` on a real
    // `ImapProviderFactory` -- only the functions it calls. `for_test`
    // (this struct's own doc comment) is what makes that possible now:
    // `secrets`/`oauth` are injected, `db_path` and the fake IMAP server's
    // TCP port are real, and the account row itself carries that port.

    /// Seeds one account row (with real `imap_host`/`imap_port`/
    /// `imap_security` columns, unlike `seed_account_and_folder`/
    /// `seed_message_with_remote_folder` above, which never needed
    /// `Core::account_connection` to resolve to anything dialable) plus
    /// one folder/thread/message row set, so a `MarkRead` op against
    /// `thread-1` -- `op()`'s own hardcoded target -- resolves through
    /// `Core`'s real `RemoteLocator` impl to a real `(folder, uid)` pair,
    /// the same shape `store_flag` (`crates/providers/src/apply.rs`)
    /// needs to issue a real `SELECT` + `UID STORE` against the fake
    /// server. Account id is always `"acct"`, matching `op()`'s own
    /// hardcoded `account_id`, so the same `op()` helper every other test
    /// in this file uses can drive `apply()` here too.
    fn seed_connectable_account(db_path: &std::path::Path, provider: &str, imap_port: u16) {
        let acc = "acct";
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO accounts \
             (id, name, email, provider, imap_host, imap_port, imap_security, \
              smtp_host, smtp_port, smtp_security, status, download_policy, \
              created_at, updated_at) \
             VALUES (?1, ?1, 'a@example.com', ?2, '127.0.0.1', ?3, 'None', \
                     '127.0.0.1', 1, 'None', 'synced', 'recent', 0, 0)",
            rusqlite::params![acc, provider, imap_port],
        )
        .unwrap();
        let folder = format!("{acc}:inbox");
        conn.execute(
            "INSERT OR IGNORE INTO folders (id, account_id, remote_id, name, kind) \
             VALUES (?1, ?2, 'Inbox', 'Inbox', 'inbox')",
            rusqlite::params![folder, acc],
        )
        .unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO threads \
             (id, account_id, folder_id, subject, snippet, date, unread) \
             VALUES ('thread-1', ?1, ?2, 'Hello', 'Hi', 0, 0)",
            rusqlite::params![acc, folder],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (id, account_id, thread_id, folder_id, provider_uid, date) \
             VALUES ('msg-1', ?1, 'thread-1', ?2, 1, 0)",
            rusqlite::params![acc, folder],
        )
        .unwrap();
    }

    /// Fixed-response [`TokenRefresh`] double (T-083, third review round):
    /// no HTTP, always returns the same access token, and refuses a
    /// second call itself -- same self-enforcing shape as `CountingReauth`/
    /// `FakeReauth` above, just one layer further out (this is what
    /// `OauthReauth::reauthenticate` actually calls, not `reauthenticate`
    /// itself).
    struct FixedTokenRefresh {
        access_token: String,
        // `Arc<AtomicU32>`, not this file's usual `Rc<Cell<u32>>`
        // (`CountingReauth`/`FakeReauth` above): unlike those, this type
        // gets boxed into `Box<dyn TokenRefresh>` and stored inside
        // `FakeOauthClientSource`, which -- via `OauthClientSource: Send`
        // (see that trait's own doc comment: `ImapProviderFactory` must
        // stay `Send`, the same as it always was, to satisfy
        // `ProviderFactory: Send`) -- must itself be `Send`. `Rc`/`Cell`
        // are never `Send`; an atomic is both `Send` and `Sync` and is
        // just as simple to assert on here.
        calls: Arc<AtomicU32>,
    }

    impl TokenRefresh for FixedTokenRefresh {
        fn refresh(&self, _refresh_token: &str) -> Result<TokenSet, OauthError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            assert!(
                n <= 1,
                "TokenRefresh::refresh() called {n} times -- the bounded, \
                 at-most-once reauth is gone"
            );
            Ok(TokenSet {
                access_token: self.access_token.clone(),
                refresh_token: None,
                expires_in: Some(3600),
            })
        }
    }

    /// Test [`OauthClientSource`]: hands out a [`FixedTokenRefresh`] for
    /// either provider, sharing one call counter between them (a given
    /// test only ever exercises one of `google`/`microsoft`, so which one
    /// shares the counter is not load-bearing, only convenient).
    struct FakeOauthClientSource {
        fresh_access_token: String,
        refresh_calls: Arc<AtomicU32>,
    }

    impl FakeOauthClientSource {
        fn token_refresh(&self) -> Box<dyn TokenRefresh> {
            Box::new(FixedTokenRefresh {
                access_token: self.fresh_access_token.clone(),
                calls: self.refresh_calls.clone(),
            })
        }
    }

    impl OauthClientSource for FakeOauthClientSource {
        fn google(&self) -> Result<Box<dyn TokenRefresh>, ConnectError> {
            Ok(self.token_refresh())
        }

        fn microsoft(&self) -> Result<Box<dyn TokenRefresh>, ConnectError> {
            Ok(self.token_refresh())
        }
    }

    /// Test [`OauthClientSource`] for the Generic (password) end-to-end
    /// test below: panics if either method is ever called, the same
    /// self-enforcing shape as `NoReauth`/`NeverReauth` above -- a Generic
    /// account has no OAuth client to load at all, so `connect()` must
    /// never even ask this for one.
    struct PanicOauthClientSource;

    impl OauthClientSource for PanicOauthClientSource {
        fn google(&self) -> Result<Box<dyn TokenRefresh>, ConnectError> {
            panic!(
                "OauthClientSource::google() was called for a Generic \
                 (password) account -- connect() must never load an OAuth \
                 client for one"
            );
        }

        fn microsoft(&self) -> Result<Box<dyn TokenRefresh>, ConnectError> {
            panic!(
                "OauthClientSource::microsoft() was called for a Generic \
                 (password) account -- connect() must never load an OAuth \
                 client for one"
            );
        }
    }

    /// Shared body for the two provider-specific "`connect()` survives a
    /// stale OAuth token" tests below (Gmail / Microsoft) -- T-083, third
    /// review round. Setup and assertions are identical end to end; the
    /// only thing that varies is which `provider` string gets written
    /// into the seeded account row (`seed_connectable_account`) and
    /// therefore, through `ConnectorKind::from_provider_str` inside
    /// `connect()` itself, which `OauthClientSource` method
    /// (`google`/`microsoft`) production code must call to reach
    /// `FakeOauthClientSource`'s shared counter.
    ///
    /// Deliberately still two `#[test]` callers below, not one test
    /// looping over both `provider` values: the mutation this test exists
    /// to catch (see `ImapProviderFactory::connect`'s own `match`'s doc
    /// comment) only ever breaks *one* `match` arm at a time. A single
    /// test covering both providers would still go red under that
    /// mutation, but it would leave ambiguous which branch broke -- and
    /// disambiguating that is exactly what a failing test's *name* is for.
    /// Two names also mean a mutation to the `Gmail` arm cannot be masked
    /// by the `Microsoft` arm still working, or vice versa (see this
    /// module's `#[cfg(test)]` self-check for both directions verified by
    /// hand).
    fn assert_connect_survives_a_stale_oauth_token_via_exactly_one_reauth(provider: &str) {
        let (port, state) = spawn_fake_imap_server(vec![], true);
        state
            .lock()
            .unwrap()
            .accept_xoauth2_token("fresh-access-token");
        thread::sleep(Duration::from_millis(30));

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("core.sqlite3");
        {
            Core::open(&db_path).unwrap();
        } // creates the schema
        seed_connectable_account(&db_path, provider, port);

        let secrets: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::new());
        secrets
            .put(&SecretKey::oauth_access("acct"), "stale-access-token")
            .unwrap();
        secrets
            .put(&SecretKey::oauth_refresh("acct"), "rt-good")
            .unwrap();

        let refresh_calls = Arc::new(AtomicU32::new(0));
        let oauth: Box<dyn OauthClientSource> = Box::new(FakeOauthClientSource {
            fresh_access_token: "fresh-access-token".into(),
            refresh_calls: refresh_calls.clone(),
        });

        let mut factory = ImapProviderFactory::for_test(db_path.clone(), secrets.clone(), oauth);
        let account_id = AccountId("acct".into());

        // The property this test exists for: `connect()` -- the exact
        // function `crate::worker` calls, on a real `ImapProviderFactory`,
        // with a real fake IMAP server, a real saved (stale) secret, and a
        // real `Core` database -- must itself self-heal the stale token,
        // not just the narrower functions it is built from.
        let mut session = factory.connect(&account_id).expect(
            "connect() must self-heal a stale OAuth access token via \
             exactly one reauth attempt, through the real \
             ImapProviderFactory::connect entry point -- not just through \
             build_session/connect_with_bounded_reauth called directly",
        );

        assert_eq!(
            refresh_calls.load(Ordering::SeqCst),
            1,
            "connect() must have refreshed the token exactly once"
        );

        session.apply(&op("op-1")).expect(
            "apply() must succeed against the real session connect() \
             handed back, proving that session was built with the fresh \
             token, not the stale one",
        );

        let stored = secrets
            .get(&SecretKey::oauth_access("acct"))
            .unwrap()
            .unwrap();
        assert_eq!(
            stored.expose(),
            "fresh-access-token",
            "the fresh token must be persisted to the (test) secret store, \
             not just used once"
        );

        assert!(
            state.lock().unwrap().mailboxes.contains_key("Inbox"),
            "the fake server never saw a SELECT for Inbox -- apply() did \
             not reach the real session"
        );

        let _ = std::fs::remove_file(&db_path);
    }

    #[test]
    fn imap_provider_factory_connect_survives_a_stale_gmail_token_via_exactly_one_reauth() {
        assert_connect_survives_a_stale_oauth_token_via_exactly_one_reauth("gmail");
    }

    #[test]
    fn imap_provider_factory_connect_survives_a_stale_microsoft_token_via_exactly_one_reauth() {
        assert_connect_survives_a_stale_oauth_token_via_exactly_one_reauth("microsoft");
    }

    #[test]
    fn imap_provider_factory_connect_never_reauths_a_generic_password_account() {
        let (port, _state) = spawn_fake_imap_server(vec![], true);
        thread::sleep(Duration::from_millis(30));

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("core.sqlite3");
        {
            Core::open(&db_path).unwrap();
        } // creates the schema
        seed_connectable_account(&db_path, "imap", port);

        let secrets: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::new());
        secrets
            .put(&SecretKey::password("acct"), "hunter2")
            .unwrap();

        let oauth: Box<dyn OauthClientSource> = Box::new(PanicOauthClientSource);
        let mut factory = ImapProviderFactory::for_test(db_path.clone(), secrets, oauth);
        let account_id = AccountId("acct".into());

        // `"imap"` is not `"generic"` -- `ConnectorKind::from_provider_str`
        // falls back to `Generic` for any unrecognized string, so this
        // also proves that fallback (not just the literal `"generic"`
        // value) reaches connect() with no reauth wrapping.
        let mut session = factory.connect(&account_id).expect(
            "a Generic account with a saved password must connect on the \
             first attempt -- the fake server's LOGIN handler always \
             succeeds",
        );

        session
            .apply(&op("op-1"))
            .expect("apply() must succeed for a Generic account too");

        // No assertion needed on a reauth call counter here: `oauth` is
        // `PanicOauthClientSource`, so if `connect()` had routed this
        // account through `connect_oauth` at all, the test would already
        // have panicked above, before ever reaching this line.

        let _ = std::fs::remove_file(&db_path);
    }
}
