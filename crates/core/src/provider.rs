//! Apply a queued operation on a remote mailbox (T-008).
//! IMAP/SMTP connect is [`MailConnector`] (T-018); the live impl lives in
//! `feathermail-providers`.

use crate::error::{CoreError, ErrorCode};
use crate::mailbox::MailboxForm;
use crate::model::{AccountId, Operation};

/// Side-effect on the provider. Local SQLite is already updated by [`crate::Core::dispatch`].
pub trait MailProvider {
    fn apply(&mut self, op: &Operation) -> Result<(), ApplyError>;
}

/// One message's current IMAP coordinates (T-025): which mailbox it lives
/// in right now, and its UID there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteMessage {
    pub folder: String,
    pub uid: u32,
}

/// Local lookups `feathermail_providers::ImapMailProvider` needs but does
/// not perform itself (D9). `thread_messages` answers "what does this local
/// thread currently map to on the server" (a thread can straddle folders if
/// a previous move only partially landed); `remote_folder` answers "what is
/// the IMAP mailbox path for this destination", where `folder_key` is
/// either a real local folder id (as carried in a `Move` operation's
/// payload) or a well-known system key (`"archive"`) for operations — like
/// `Archive` — that target a system folder without naming one explicitly.
///
/// This trait is declared here rather than in `feathermail-providers`
/// (which is where `MailProvider::apply` calls it) because its
/// implementation (T-075, [`crate::locator`]) needs `Core`'s private SQLite
/// handle: `crates/providers` depends on `crates/core`, so the reverse
/// dependency an impl-over-`Core` would need cannot exist there without a
/// cycle (D9). `feathermail-providers` re-exports these names so its own
/// callers see no difference.
pub trait RemoteLocator {
    fn thread_messages(
        &self,
        account_id: &AccountId,
        thread_id: &str,
    ) -> Result<Vec<RemoteMessage>, ApplyError>;

    /// Resolve the source coordinates captured for one concrete queued
    /// operation.  A thread can have more than one pending move by the time
    /// a worker gets to it; using the thread-wide view in that case would
    /// combine the source UIDs of unrelated operations and send both moves
    /// against whichever operation happened to be claimed first.  The
    /// default keeps legacy locators (and callers with no durable move
    /// intent) working; Core overrides it with the operation-scoped lookup.
    fn thread_messages_for_operation(
        &self,
        account_id: &AccountId,
        thread_id: &str,
        _operation_id: &str,
    ) -> Result<Vec<RemoteMessage>, ApplyError> {
        self.thread_messages(account_id, thread_id)
    }

    fn remote_folder(&self, account_id: &AccountId, folder_key: &str)
        -> Result<String, ApplyError>;

    /// Resolve the destination captured for one concrete queued move.  The
    /// fallback is intentional for pre-T-076/legacy operations that have no
    /// `operation_moves` row.
    fn remote_folder_for_operation(
        &self,
        account_id: &AccountId,
        folder_key: &str,
        _operation_id: &str,
    ) -> Result<String, ApplyError> {
        self.remote_folder(account_id, folder_key)
    }
}

/// Well-known [`RemoteLocator::remote_folder`] key for `Archive` operations
/// (which carry no folder id of their own, unlike `Move`).
pub const ARCHIVE_FOLDER_KEY: &str = "archive";

/// Well-known [`RemoteLocator::remote_folder`] key for `Trash` operations
/// (same shape as [`ARCHIVE_FOLDER_KEY`]: `Trash` carries no folder id of
/// its own either).
pub const TRASH_FOLDER_KEY: &str = "trash";

/// Probe IMAP LOGIN + SMTP before saving an account (T-018). Password is not stored.
pub trait MailConnector {
    fn probe(&self, form: &MailboxForm, password: &str) -> Result<ConnectOk, ConnectError>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectOk {
    pub capabilities: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectError {
    Auth {
        message: String,
        details: Option<String>,
    },
    Network {
        message: String,
        details: Option<String>,
    },
    Invalid {
        message: String,
        details: Option<String>,
    },
}

impl ConnectError {
    pub fn auth(details: impl Into<String>) -> Self {
        Self::Auth {
            message: "That password wasn't accepted.".into(),
            details: Some(details.into()),
        }
    }

    /// Revoked / expired OAuth (T-019). Not a password typo; do not tight-retry.
    pub fn reauth(details: impl Into<String>) -> Self {
        Self::Auth {
            message: ErrorCode::AuthRequired.default_message().into(),
            details: Some(details.into()),
        }
    }

    pub fn network(details: impl Into<String>) -> Self {
        Self::Network {
            message: ErrorCode::NetworkUnavailable.default_message().into(),
            details: Some(details.into()),
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid {
            message: message.into(),
            details: None,
        }
    }
}

impl From<ConnectError> for CoreError {
    fn from(err: ConnectError) -> Self {
        match err {
            ConnectError::Auth { message, details } => {
                let e = CoreError::new(ErrorCode::AuthRequired, message);
                match details {
                    Some(d) => e.with_details(d),
                    None => e,
                }
            }
            ConnectError::Network { message, details } => {
                let e = CoreError::new(ErrorCode::NetworkUnavailable, message);
                match details {
                    Some(d) => e.with_details(d),
                    None => e,
                }
            }
            ConnectError::Invalid { message, details } => {
                let e = CoreError::new(ErrorCode::InvalidArgument, message);
                match details {
                    Some(d) => e.with_details(d),
                    None => e,
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplyError {
    Network,
    Auth,
    /// Same flag already on the server (D29) — worker ACKs.
    Conflict,
    NotFound,
    Unsupported,
    /// T-060u: the server refused to destroy a mailbox that still holds
    /// mail. Terminal, never retried: Core only ever queues a folder
    /// deletion for a folder its own mirror saw as empty, so reaching
    /// this means mail arrived between the local check and the wire
    /// call. Retrying would only wait for that mail to be deleted by
    /// someone else; the honest outcome is to fail the operation and let
    /// the next `LIST` bring the folder back (see
    /// `feathermail_core::remote::sync_one_folder`).
    NotEmpty,
}

impl ApplyError {
    pub fn code(self) -> ErrorCode {
        match self {
            Self::Network => ErrorCode::NetworkUnavailable,
            Self::Auth => ErrorCode::AuthRequired,
            Self::Conflict => ErrorCode::Conflict,
            Self::NotFound => ErrorCode::MessageNotFound,
            Self::Unsupported => ErrorCode::OperationNotSupported,
            Self::NotEmpty => ErrorCode::Conflict,
        }
    }

    pub fn retry(self) -> bool {
        matches!(self, Self::Network)
    }
}

/// T-083: attempts, at most once per `Auth` failure, to fix whatever caused
/// it and hand back a provider built from a fresh session/credential.
///
/// ## Why this is not a second `ApplyError::Auth` variant
///
/// The obvious move for "expired token" vs. "revoked token" is to split
/// `Auth` into two variants. That only works if *something on the wire*
/// can tell the two apart at the moment the error happens. It can't:
/// `crates/providers/src/apply.rs::connect_to_apply` turns *any*
/// `ConnectError::Auth` -- an IMAP `NO`/`BAD` on login, whatever the
/// server's reason -- into the same `ApplyError::Auth`, because IMAP does
/// not hand back a machine-readable "expired" vs. "revoked" distinction.
/// So the fork is not in the error's shape; it is in *who tries what,
/// once*, after the error: [`ReauthingProvider::apply`] calls
/// [`Reauthenticate::reauthenticate`] the first time (and only the first
/// time) a call to the wrapped provider's `apply` comes back `Auth`, and
/// retries the same operation exactly once against whatever came back.
///
/// - If that retry is not `Auth`, the failure was the fixable kind (an
///   expired access token, refreshed via `refresh_token`) and the caller
///   (`Core::tick`) never sees `Auth` at all: no rollback, no
///   `accounts.status = 'error'`, no user-visible signal, because there is
///   nothing for the user to react to.
/// - If it is still `Auth`, or `reauthenticate` itself could not produce a
///   working session (no `refresh_token` saved, the refresh call itself
///   got `invalid_grant`, or the fresh session was rejected again after a
///   token that did refresh successfully), `Auth` really was terminal and
///   is passed straight through unchanged: `Core::tick` rolls the
///   optimistic mark back and sets `accounts.status = 'error'`, exactly as
///   it did before T-083. That status flip plus the `TickOutcome::Failed`
///   event `worker.rs` already emits on it *is* the "tell the user, don't
///   just make the mark vanish" channel -- T-083 does not need to invent a
///   new one, only make sure it still fires for the cases that are
///   genuinely terminal and stops firing (stops rolling back!) for the
///   ones that are not.
///
/// ## Why "at most once" needs no counter
///
/// A naive `loop { retry; if auth { reauth again } }` is "hammer the token
/// endpoint until banned," which is exactly what the task forbids. This
/// type never loops: [`ReauthingProvider::apply`] is two sequential,
/// straight-line calls to the inner provider's `apply` -- one before any
/// reauth attempt, one after -- with no branch that goes back for a third.
/// A [`Reauthenticate`] impl that is itself buggy and always reports
/// success without fixing anything still cannot spin this wrapper: the
/// second `apply` call either isn't `Auth` (done) or is (terminal, passed
/// through). See `reauth_attempted_at_most_once_per_apply_call` below,
/// which pins this down with a fake that always "succeeds" and always
/// hands back a still-broken provider.
///
/// Retries *across* ticks (e.g. the refresh call itself failing with a
/// network error, or the whole account's operations queued behind the
/// first `Auth`) are not this type's concern: [`ApplyError::Network`] from
/// either the refresh call or the reconnect propagates straight through
/// `apply()` unchanged, and `Core::tick`'s existing `err.retry()` branch
/// re-queues it with D32's existing backoff (`retry_delay_secs`). No
/// second backoff mechanism is introduced here.
pub trait Reauthenticate<P> {
    fn reauthenticate(&mut self) -> Result<P, ApplyError>;
}

/// Wraps any [`MailProvider`] with the bounded, at-most-once-per-`apply`-
/// call reauth attempt described on [`Reauthenticate`] (T-083).
pub struct ReauthingProvider<P, R> {
    inner: P,
    reauth: R,
}

impl<P, R> ReauthingProvider<P, R> {
    pub fn new(inner: P, reauth: R) -> Self {
        Self { inner, reauth }
    }

    /// Reaches the wrapped, `Sized` provider (T-083). `crates/service`'s
    /// `MailSession` trait has three forwarding methods
    /// (`sync_one_folder`/`open_one_body`/`idle_once`) that each need a
    /// concrete `&mut ImapMailProvider<Core>` to monomorphize a generic
    /// callee against (`feathermail_sync::sync_folder<M: MailboxSession>`
    /// and friends) -- that concrete reference can only come from *inside*
    /// a type that actually holds one, never through a `&mut dyn`
    /// accessor. `ReauthingProvider`'s own impl of `MailSession` (in
    /// `crates/service/src/provider_factory.rs`) needs exactly that door
    /// in, and `inner`/`reauth` are private for good reason (nothing
    /// outside this module should be able to swap either one out from
    /// under `apply`'s bookkeeping), so this accessor is the narrow hole
    /// punched through for that one caller.
    ///
    /// Do **not** use this to bypass [`MailProvider::apply`] for anything
    /// that can fail with [`ApplyError::Auth`]: `apply` is the only place
    /// the bounded, at-most-once reauth attempt described on
    /// [`Reauthenticate`] ever runs. Driving the inner provider directly
    /// through this accessor for an operation that can hit `Auth` skips
    /// that retry entirely -- the caller just gets a bare auth failure
    /// instead of the one free reauth this wrapper exists to give it.
    /// (This is exactly why
    /// `MailSession::sync_one_folder`'s own doc comment on the impl in
    /// `provider_factory.rs` has to spell out, honestly, that inbound sync
    /// does not get that retry today -- `SyncError` has no `Auth` variant
    /// to catch, not because this accessor is unsafe to call, but because
    /// nothing on the other end of it currently would.)
    pub fn inner_mut(&mut self) -> &mut P {
        &mut self.inner
    }
}

impl<P: MailProvider, R: Reauthenticate<P>> MailProvider for ReauthingProvider<P, R> {
    fn apply(&mut self, op: &Operation) -> Result<(), ApplyError> {
        match self.inner.apply(op) {
            Err(ApplyError::Auth) => match self.reauth.reauthenticate() {
                // Only swap `inner` in on success: on failure the old,
                // still-intact provider is left in place untouched, so a
                // later tick (e.g. after a network hiccup on the refresh
                // call clears up) has something sane to keep using.
                Ok(fresh) => {
                    self.inner = fresh;
                    self.inner.apply(op)
                }
                Err(err) => Err(err),
            },
            other => other,
        }
    }
}

#[cfg(test)]
mod reauth_tests {
    use super::*;

    /// A [`MailProvider`] fake that fails every `apply()` with `Auth`
    /// until told otherwise, then always succeeds. Records every op it was
    /// actually asked to apply so tests can assert nothing was silently
    /// skipped or duplicated.
    struct FakeProvider {
        fixed: bool,
        applied: Vec<String>,
    }

    impl FakeProvider {
        fn broken() -> Self {
            Self {
                fixed: false,
                applied: Vec::new(),
            }
        }

        fn fixed() -> Self {
            Self {
                fixed: true,
                applied: Vec::new(),
            }
        }
    }

    impl MailProvider for FakeProvider {
        fn apply(&mut self, op: &Operation) -> Result<(), ApplyError> {
            self.applied.push(op.id.0.clone());
            if self.fixed {
                Ok(())
            } else {
                Err(ApplyError::Auth)
            }
        }
    }

    /// Reauth fake whose behavior is scripted per test: how many times it
    /// was called is always recorded so the "at most once" bound can be
    /// asserted directly, not just inferred from the outcome.
    struct ScriptedReauth {
        calls: usize,
        outcome: fn() -> Result<FakeProvider, ApplyError>,
    }

    impl ScriptedReauth {
        fn new(outcome: fn() -> Result<FakeProvider, ApplyError>) -> Self {
            Self { calls: 0, outcome }
        }
    }

    impl Reauthenticate<FakeProvider> for ScriptedReauth {
        fn reauthenticate(&mut self) -> Result<FakeProvider, ApplyError> {
            self.calls += 1;
            // A regression that turns `apply()`'s straight-line retry back
            // into a loop must make the *test* fail by name, not hang the
            // run -- a hung `cargo test` looks like nothing broke instead
            // of saying what broke and where (this bit us before, see
            // T-078 (b)). So the double itself refuses a second call
            // rather than waiting to be asked how many times it was
            // called after `apply()` eventually returns -- with a real
            // loop, it never does.
            assert!(
                self.calls <= 1,
                "reauthenticate() called {} times in one apply() -- the \
                 at-most-once bound is gone, and a real server would be \
                 hammered until it bans us",
                self.calls
            );
            (self.outcome)()
        }
    }

    fn op(id: &str) -> Operation {
        Operation {
            id: crate::model::OperationId(id.to_string()),
            account_id: AccountId("acct".into()),
            target_id: "thread-1".into(),
            kind: crate::model::OpKind::MarkRead,
            payload: "{}".into(),
            payload_hash: "hash".into(),
            created_at: 0,
            retry_count: 0,
            next_attempt_at: None,
            status: crate::model::OpStatus::Pending,
            undo_of: None,
        }
    }

    #[test]
    fn successful_reauth_recovers_transparently() {
        let mut provider = ReauthingProvider::new(
            FakeProvider::broken(),
            ScriptedReauth::new(|| Ok(FakeProvider::fixed())),
        );

        let result = provider.apply(&op("op-1"));

        assert_eq!(result, Ok(()));
        assert_eq!(provider.reauth.calls, 1);
        // Applied twice against the *broken* provider (the first failing
        // attempt, then the retry that -- because `reauthenticate` swaps
        // in a fresh provider -- actually lands on the fixed one instead).
        assert_eq!(provider.inner.applied, vec!["op-1".to_string()]);
    }

    #[test]
    fn reauth_failure_propagates_auth_as_terminal() {
        let mut provider = ReauthingProvider::new(
            FakeProvider::broken(),
            ScriptedReauth::new(|| Err(ApplyError::Auth)),
        );

        let result = provider.apply(&op("op-1"));

        assert_eq!(result, Err(ApplyError::Auth));
        assert_eq!(provider.reauth.calls, 1);
    }

    #[test]
    fn reauth_network_failure_propagates_as_retryable() {
        // e.g. the token endpoint itself is unreachable: D32's existing
        // backoff picks this up through `ApplyError::Network.retry()`
        // rather than this wrapper inventing a second mechanism.
        let mut provider = ReauthingProvider::new(
            FakeProvider::broken(),
            ScriptedReauth::new(|| Err(ApplyError::Network)),
        );

        let result = provider.apply(&op("op-1"));

        assert_eq!(result, Err(ApplyError::Network));
        assert!(result.unwrap_err().retry());
    }

    #[test]
    fn reauth_attempted_at_most_once_per_apply_call() {
        // A `Reauthenticate` that always claims success but always hands
        // back a still-broken provider -- the "buggy reauth" case the
        // wrapper itself must stay safe against. If `apply()` ever grew a
        // loop back to `reauthenticate()` instead of returning after the
        // single retry, `ScriptedReauth::reauthenticate`'s own `assert!`
        // (see above) panics on the second call, so this test fails by
        // name and immediately -- it does not rely on a hang, which is
        // exactly the shape T-083 was warned against ("обновить и
        // повторить без счётчика -- способ долбить сервер до бана").
        let mut provider = ReauthingProvider::new(
            FakeProvider::broken(),
            ScriptedReauth::new(|| Ok(FakeProvider::broken())),
        );

        let result = provider.apply(&op("op-1"));

        assert_eq!(result, Err(ApplyError::Auth));
        assert_eq!(
            provider.reauth.calls, 1,
            "reauthenticate() must be called exactly once per apply() call, never more"
        );
    }

    #[test]
    fn non_auth_errors_never_trigger_reauth() {
        struct AlwaysNotFound;
        impl MailProvider for AlwaysNotFound {
            fn apply(&mut self, _op: &Operation) -> Result<(), ApplyError> {
                Err(ApplyError::NotFound)
            }
        }

        struct UnusedReauth {
            calls: usize,
        }
        impl Reauthenticate<AlwaysNotFound> for UnusedReauth {
            fn reauthenticate(&mut self) -> Result<AlwaysNotFound, ApplyError> {
                self.calls += 1;
                Ok(AlwaysNotFound)
            }
        }

        let mut provider = ReauthingProvider::new(AlwaysNotFound, UnusedReauth { calls: 0 });

        let result = provider.apply(&op("op-1"));

        assert_eq!(result, Err(ApplyError::NotFound));
        assert_eq!(provider.reauth.calls, 0);
    }
}
