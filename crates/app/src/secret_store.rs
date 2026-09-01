//! T-088: the single door between `crates/app` and a concrete secret-store
//! backend. Nothing else in this crate is allowed to name
//! `feathermail_security::LibsecretStore` (the real system keyring) or
//! `feathermail_security::MemorySecretStore` directly -- `shell.rs` only
//! ever holds a [`SessionSecrets`] and calls [`SessionSecrets::connect`]
//! on it. `scripts/check-secret-store-single-door.sh` enforces that
//! nothing under `crates/app/src` mentions the real keyring backend
//! outside this file, so a call site that reverts to talking to the
//! keyring directly (the exact regression T-088 was filed over: an
//! ephemeral session's password landing permanently in the system
//! keyring under an `account_id` nothing can ever delete again) needs a
//! new, visible import here instead of a one-line edit somewhere the
//! backend type already happens to be in scope.

use std::sync::Arc;

use feathermail_security::{LibsecretStore, MemorySecretStore, SecretError, SecretStore};

/// Which secret store answers for one session's account passwords --
/// decided once, by `crate::shell::profile_open_effect`, from the same
/// verdict that already decides whether the sync worker starts and
/// whether the ephemeral banner shows (see that function's doc comment).
/// That single-decision-point discipline is why this type exposes only
/// two constructors (below) and nothing that lets a caller pick a
/// backend by hand.
///
/// `OnDisk` carries nothing: an on-disk session still connects to the
/// real Secret Service freshly, per operation
/// (`LibsecretStore::connect()`), off the GTK thread, exactly as before
/// T-088 -- a keyring that is briefly unavailable at startup can still
/// work moments later, and this must not change that. `Ephemeral` carries
/// one `MemorySecretStore`, constructed once and shared (via `Arc`) by
/// every clone of this value for the rest of the session, so a password
/// `save_mailbox_secret` writes is still readable by whatever reads it
/// back later in the same run -- an ephemeral session must keep working
/// *within itself*; only a process restart may not recover it.
#[derive(Clone)]
pub(crate) enum SessionSecrets {
    OnDisk,
    Ephemeral(Arc<MemorySecretStore>),
}

impl SessionSecrets {
    /// A session backed by a real on-disk profile.
    pub(crate) fn on_disk() -> Self {
        Self::OnDisk
    }

    /// A session backed by `Core::memory()`: a fresh, session-scoped
    /// in-memory store that is never the real system keyring.
    pub(crate) fn ephemeral() -> Self {
        Self::Ephemeral(Arc::new(MemorySecretStore::new()))
    }

    /// Connects to whichever store this session actually uses.
    /// `LibsecretStore::connect()` talks to D-Bus and can fail (no
    /// keyring service on the bus); the ephemeral branch never does --
    /// there is nothing to fail to reach, which is also why adding an
    /// account in an ephemeral session must not become *less* reliable
    /// than it was before T-088.
    pub(crate) fn connect(&self) -> Result<Arc<dyn SecretStore>, SecretError> {
        match self {
            Self::OnDisk => {
                LibsecretStore::connect().map(|store| Arc::new(store) as Arc<dyn SecretStore>)
            }
            Self::Ephemeral(store) => Ok(Arc::clone(store) as Arc<dyn SecretStore>),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use feathermail_security::SecretKey;

    /// T-088's within-session bar restated at this type's own boundary
    /// (`shell::tests` restates it again at `profile_open_effect`'s):
    /// connecting must never fail, and a password written through one
    /// `.connect()` call must still be there for a later one -- e.g.
    /// `Msg::RemoveAccountConfirm` reconnecting moments after
    /// `save_mailbox_secret` wrote the password.
    #[test]
    fn ephemeral_connect_never_fails_and_keeps_what_it_stores() {
        let secrets = SessionSecrets::ephemeral();
        let store = secrets
            .connect()
            .expect("an ephemeral session's own store must never refuse a connection");
        store.put(&SecretKey::password("acc-1"), "hunter2").unwrap();

        let store_again = secrets.connect().unwrap();
        assert_eq!(
            store_again
                .get(&SecretKey::password("acc-1"))
                .unwrap()
                .unwrap()
                .expose(),
            "hunter2",
            "a password saved earlier in this session must still be readable later in it"
        );
    }

    /// An on-disk session must route to the real Secret Service, not the
    /// in-memory fallback -- proven the only way possible without a live
    /// D-Bus session in the test sandbox: connecting either succeeds
    /// (a keyring is actually on the bus) or fails with `Unavailable`
    /// (D14's fail-closed contract), never silently falling back to an
    /// in-memory store that would make `SessionSecrets::OnDisk` behave
    /// like `SessionSecrets::Ephemeral`.
    #[test]
    fn on_disk_connect_reaches_the_real_keyring_or_fails_closed_never_falls_back_in_memory() {
        let secrets = SessionSecrets::on_disk();
        match secrets.connect() {
            Ok(store) => {
                // A real keyring is reachable in this sandbox: prove it
                // is genuinely the shared system store, not a private
                // in-memory one, by writing through one handle and
                // reading through a second, independently-connected one.
                let id = format!(
                    "t088-{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos()
                );
                let key = SecretKey::password(&id);
                let result = (|| {
                    store.put(&key, "hunter2")?;
                    let other = secrets.connect()?;
                    let got = other.get(&key)?.ok_or_else(|| SecretError::Backend {
                        message: "put did not roundtrip through a second connection".into(),
                    })?;
                    if got.expose() != "hunter2" {
                        return Err(SecretError::Backend {
                            message: "put/get mismatch across connections".into(),
                        });
                    }
                    other.delete(&key)
                })();
                let _ = store.delete(&key);
                result.unwrap();
            }
            Err(err) => assert!(
                err.is_unavailable(),
                "a failure to reach the real keyring must be D14's fail-closed \
                 `Unavailable`, not some other error that could be confused with a \
                 deliberate in-memory fallback: {err:?}"
            ),
        }
    }
}
