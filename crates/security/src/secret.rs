//! Secret Service storage for passwords and OAuth tokens (T-010, D14, §53).
//!
//! Talks the same D-Bus API libsecret uses (GNOME Keyring, KWallet). Never
//! writes secrets to SQLite. If the service is missing, fail closed.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use secret_service::blocking::SecretService;
use secret_service::{EncryptionType, Error as SsError};

use crate::redact::REDACTED;

/// Desktop id; also the Secret Service `application` attribute.
pub const APP_ID: &str = "app.feathermail.FeatherMail";

const ATTR_APPLICATION: &str = "application";
const ATTR_ACCOUNT: &str = "account_id";
const ATTR_KIND: &str = "kind";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SecretKind {
    Password,
    OauthAccess,
    OauthRefresh,
}

impl SecretKind {
    pub const ALL: [Self; 3] = [Self::Password, Self::OauthAccess, Self::OauthRefresh];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Password => "password",
            Self::OauthAccess => "oauth_access",
            Self::OauthRefresh => "oauth_refresh",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SecretKey {
    pub account_id: String,
    pub kind: SecretKind,
}

impl SecretKey {
    pub fn new(account_id: impl Into<String>, kind: SecretKind) -> Self {
        Self {
            account_id: account_id.into(),
            kind,
        }
    }

    pub fn password(account_id: impl Into<String>) -> Self {
        Self::new(account_id, SecretKind::Password)
    }

    pub fn oauth_access(account_id: impl Into<String>) -> Self {
        Self::new(account_id, SecretKind::OauthAccess)
    }

    pub fn oauth_refresh(account_id: impl Into<String>) -> Self {
        Self::new(account_id, SecretKind::OauthRefresh)
    }
}

/// Secret that redacts in [`Debug`]. Use [`expose`](Self::expose) only at IMAP/OAuth.
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SecretString({REDACTED})")
    }
}

impl PartialEq for SecretString {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for SecretString {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SecretError {
    /// D14: no plaintext fallback into SQLite.
    Unavailable {
        message: String,
    },
    Invalid {
        message: String,
    },
    Backend {
        message: String,
    },
}

impl SecretError {
    pub fn unavailable() -> Self {
        Self::Unavailable {
            message: "The system keyring isn't available. Feather Mail won't save the password in the mailbox file.".into(),
        }
    }

    pub fn is_unavailable(&self) -> bool {
        matches!(self, Self::Unavailable { .. })
    }
}

impl fmt::Display for SecretError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable { message }
            | Self::Invalid { message }
            | Self::Backend { message } => f.write_str(message),
        }
    }
}

impl std::error::Error for SecretError {}

/// T-088: `crates/app` picks its concrete store (real keyring vs.
/// in-memory) once per session and hands callers a single `Arc<dyn
/// SecretStore>` so every call site (`save_mailbox_secret`,
/// `Core::remove_account`, ...) shares the same trait-object handle
/// instead of re-deriving "which store" itself. `Core::remove_account`
/// (and anything else generic over `impl SecretStore`) needs that handle
/// to satisfy `SecretStore` directly -- a bare `&dyn SecretStore` cannot,
/// because a generic type parameter is implicitly `Sized` and `dyn
/// SecretStore` is not -- so this blanket impl forwards through the
/// `Arc` to whatever it points at.
impl<T: SecretStore + ?Sized> SecretStore for Arc<T> {
    fn put(&self, key: &SecretKey, secret: &str) -> Result<(), SecretError> {
        (**self).put(key, secret)
    }

    fn get(&self, key: &SecretKey) -> Result<Option<SecretString>, SecretError> {
        (**self).get(key)
    }

    fn delete(&self, key: &SecretKey) -> Result<(), SecretError> {
        (**self).delete(key)
    }
}

pub trait SecretStore: Send + Sync {
    fn put(&self, key: &SecretKey, secret: &str) -> Result<(), SecretError>;
    fn get(&self, key: &SecretKey) -> Result<Option<SecretString>, SecretError>;
    fn delete(&self, key: &SecretKey) -> Result<(), SecretError>;

    /// T-021: called from `Core::remove_account` after the local SQLite
    /// transaction has already committed, so this must not bail out after
    /// the first failure -- a bug here (returning on the first `?`) would
    /// leave a long-lived OAuth refresh token in the keyring forever
    /// because `Password` happened to be first in `SecretKind::ALL` and
    /// failed to delete. Every kind is attempted regardless of whether an
    /// earlier one failed; only then is an error reported, and only for the
    /// kinds that actually failed. The message names kinds only (e.g.
    /// "password, oauth_refresh") -- never the account id (which callers
    /// may derive from an email address) and never the underlying secret.
    fn delete_account(&self, account_id: &str) -> Result<(), SecretError> {
        let mut failed = Vec::new();
        for kind in SecretKind::ALL {
            if self.delete(&SecretKey::new(account_id, kind)).is_err() {
                failed.push(kind.as_str());
            }
        }
        if failed.is_empty() {
            return Ok(());
        }
        Err(SecretError::Backend {
            message: format!(
                "Could not remove every saved credential for this account from the \
                 system keyring ({}). Some may still be present there.",
                failed.join(", ")
            ),
        })
    }
}

/// In-memory store. Originally test-only; T-088 also uses it as the
/// production secret store for an ephemeral (`Core::memory()`) session, so
/// that session's added-account passwords never reach the real system
/// keyring under an `account_id` that will not outlive the window (see
/// `feathermail::shell::SessionSecrets`). Still not a fallback for an
/// on-disk session (D14).
#[derive(Default)]
pub struct MemorySecretStore {
    inner: Mutex<HashMap<SecretKey, String>>,
}

impl MemorySecretStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn map(&self) -> Result<std::sync::MutexGuard<'_, HashMap<SecretKey, String>>, SecretError> {
        self.inner.lock().map_err(|_| SecretError::Backend {
            message: "secret store lock poisoned".into(),
        })
    }
}

impl SecretStore for MemorySecretStore {
    fn put(&self, key: &SecretKey, secret: &str) -> Result<(), SecretError> {
        check_key(key)?;
        check_secret(secret)?;
        self.map()?.insert(key.clone(), secret.to_string());
        Ok(())
    }

    fn get(&self, key: &SecretKey) -> Result<Option<SecretString>, SecretError> {
        check_key(key)?;
        Ok(self.map()?.get(key).cloned().map(SecretString::new))
    }

    fn delete(&self, key: &SecretKey) -> Result<(), SecretError> {
        check_key(key)?;
        self.map()?.remove(key);
        Ok(())
    }
}

/// D14: since T-088, this type can be reached from `crates/app` (embedded
/// in a live session), not only from tests -- unlike `LibsecretStore`
/// (which never holds a secret in the process, only a D-Bus handle), this
/// one's whole job is to hold secrets in memory, so its `Debug` must
/// redact the same way `SecretString`/`TokenSet`/`Pkce` already do,
/// rather than relying on nothing in this codebase ever deriving `Debug`
/// on a struct that embeds it.
impl fmt::Debug for MemorySecretStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemorySecretStore")
            .field("entries", &REDACTED)
            .finish()
    }
}

/// Production backend: Secret Service via D-Bus (libsecret protocol).
pub struct LibsecretStore;

impl LibsecretStore {
    /// Fails with [`SecretError::Unavailable`] when no keyring is on the bus.
    pub fn connect() -> Result<Self, SecretError> {
        let _ = session()?;
        Ok(Self)
    }
}

impl SecretStore for LibsecretStore {
    fn put(&self, key: &SecretKey, secret: &str) -> Result<(), SecretError> {
        check_key(key)?;
        check_secret(secret)?;
        let ss = session()?;
        let collection = ss.get_any_collection().map_err(map_ss)?;
        let attrs = attributes(key);
        let label = item_label(key);
        collection
            .create_item(&label, attrs, secret.as_bytes(), true, "text/plain")
            .map_err(map_ss)?;
        Ok(())
    }

    fn get(&self, key: &SecretKey) -> Result<Option<SecretString>, SecretError> {
        check_key(key)?;
        let ss = session()?;
        let found = ss.search_items(attributes(key)).map_err(map_ss)?;
        let item = if let Some(item) = found.unlocked.first() {
            item
        } else if let Some(item) = found.locked.first() {
            item.unlock().map_err(map_ss)?;
            item
        } else {
            return Ok(None);
        };
        let bytes = item.get_secret().map_err(map_ss)?;
        let value = String::from_utf8(bytes).map_err(|_| SecretError::Backend {
            message: "stored secret is not utf-8".into(),
        })?;
        Ok(Some(SecretString::new(value)))
    }

    fn delete(&self, key: &SecretKey) -> Result<(), SecretError> {
        check_key(key)?;
        let ss = session()?;
        let found = match ss.search_items(attributes(key)) {
            Ok(found) => found,
            Err(SsError::NoResult) => return Ok(()),
            Err(e) => return Err(map_ss(e)),
        };
        for item in found.unlocked.iter().chain(found.locked.iter()) {
            match item.delete() {
                Ok(()) | Err(SsError::NoResult) => {}
                Err(e) => return Err(map_ss(e)),
            }
        }
        Ok(())
    }
}

fn session() -> Result<SecretService<'static>, SecretError> {
    SecretService::connect(EncryptionType::Dh).map_err(map_ss)
}

fn check_key(key: &SecretKey) -> Result<(), SecretError> {
    if key.account_id.is_empty() {
        return Err(SecretError::Invalid {
            message: "account id is required".into(),
        });
    }
    Ok(())
}

fn check_secret(secret: &str) -> Result<(), SecretError> {
    if secret.is_empty() {
        return Err(SecretError::Invalid {
            message: "secret is empty".into(),
        });
    }
    Ok(())
}

fn attributes(key: &SecretKey) -> HashMap<&str, &str> {
    HashMap::from([
        (ATTR_APPLICATION, APP_ID),
        (ATTR_ACCOUNT, key.account_id.as_str()),
        (ATTR_KIND, key.kind.as_str()),
    ])
}

fn item_label(key: &SecretKey) -> String {
    format!("Feather Mail {} ({})", key.kind.as_str(), key.account_id)
}

fn map_ss(err: SsError) -> SecretError {
    match err {
        SsError::Unavailable | SsError::Locked | SsError::Prompt => SecretError::unavailable(),
        SsError::Zbus(_) | SsError::ZbusFdo(_) => SecretError::unavailable(),
        other => SecretError::Backend {
            message: other.to_string(),
        },
    }
}

#[cfg(test)]
struct UnavailableStore;

#[cfg(test)]
impl SecretStore for UnavailableStore {
    fn put(&self, key: &SecretKey, secret: &str) -> Result<(), SecretError> {
        check_key(key)?;
        check_secret(secret)?;
        Err(SecretError::unavailable())
    }

    fn get(&self, key: &SecretKey) -> Result<Option<SecretString>, SecretError> {
        check_key(key)?;
        Err(SecretError::unavailable())
    }

    fn delete(&self, key: &SecretKey) -> Result<(), SecretError> {
        check_key(key)?;
        Err(SecretError::unavailable())
    }
}

#[cfg(test)]
struct PartiallyFailingStore {
    inner: MemorySecretStore,
    fails: SecretKind,
}

#[cfg(test)]
impl SecretStore for PartiallyFailingStore {
    fn put(&self, key: &SecretKey, secret: &str) -> Result<(), SecretError> {
        self.inner.put(key, secret)
    }

    fn get(&self, key: &SecretKey) -> Result<Option<SecretString>, SecretError> {
        self.inner.get(key)
    }

    fn delete(&self, key: &SecretKey) -> Result<(), SecretError> {
        if key.kind == self.fails {
            return Err(SecretError::Backend {
                message: "simulated backend failure".into(),
            });
        }
        self.inner.delete(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn roundtrip<S: SecretStore>(store: &S) {
        let password = SecretKey::password("john");
        let access = SecretKey::oauth_access("john");
        let refresh = SecretKey::oauth_refresh("john");
        store.put(&password, "hunter2").unwrap();
        store.put(&access, "at-1").unwrap();
        store.put(&refresh, "rt-1").unwrap();
        assert_eq!(store.get(&password).unwrap().unwrap().expose(), "hunter2");
        assert_eq!(store.get(&access).unwrap().unwrap().expose(), "at-1");
        assert_eq!(store.get(&refresh).unwrap().unwrap().expose(), "rt-1");
        store.put(&password, "hunter3").unwrap();
        assert_eq!(store.get(&password).unwrap().unwrap().expose(), "hunter3");
        store.delete(&password).unwrap();
        assert!(store.get(&password).unwrap().is_none());
        store.delete_account("john").unwrap();
        assert!(store.get(&access).unwrap().is_none());
        assert!(store.get(&refresh).unwrap().is_none());
    }

    #[test]
    fn memory_put_get_delete_password_and_oauth() {
        roundtrip(&MemorySecretStore::new());
    }

    #[test]
    fn delete_account_tries_every_kind_even_when_one_delete_fails() {
        // T-021 follow-up: `delete_account`'s default impl used to bail on
        // the first `?`, so a failure deleting `Password` (first in
        // `SecretKind::ALL`) meant `OauthAccess`/`OauthRefresh` were never
        // even attempted -- a refresh token could outlive the account it
        // belonged to. This proves all three are attempted regardless.
        let store = PartiallyFailingStore {
            inner: MemorySecretStore::new(),
            fails: SecretKind::Password,
        };
        store.put(&SecretKey::password("john"), "hunter2").unwrap();
        store.put(&SecretKey::oauth_access("john"), "at-1").unwrap();
        store
            .put(&SecretKey::oauth_refresh("john"), "rt-1")
            .unwrap();

        let err = store.delete_account("john").unwrap_err();

        // The kind whose delete failed is (per this double) still present...
        assert!(store
            .inner
            .get(&SecretKey::password("john"))
            .unwrap()
            .is_some());
        // ...but the other two were not abandoned just because the first
        // kind in SecretKind::ALL failed.
        assert!(store
            .inner
            .get(&SecretKey::oauth_access("john"))
            .unwrap()
            .is_none());
        assert!(store
            .inner
            .get(&SecretKey::oauth_refresh("john"))
            .unwrap()
            .is_none());

        // Human message; never the account id or a secret value.
        let msg = err.to_string();
        assert!(!msg.contains("john"));
        assert!(!msg.contains("hunter2"));
        assert!(!msg.contains("at-1"));
        assert!(!msg.contains("rt-1"));
        assert!(msg.contains("password"));
    }

    #[test]
    fn memory_get_missing_is_none() {
        let store = MemorySecretStore::new();
        assert!(store
            .get(&SecretKey::password("missing"))
            .unwrap()
            .is_none());
        store.delete(&SecretKey::password("missing")).unwrap();
    }

    #[test]
    fn memory_secret_store_debug_does_not_leak() {
        // T-088: this store is now embedded in a live `App` for an
        // ephemeral session, not just built ad hoc in tests -- its
        // `Debug` must redact like `SecretString`'s does (D14), proven the
        // same way: put something secret in, format the store, the
        // secret and the account id it was filed under must not appear.
        let store = MemorySecretStore::new();
        store
            .put(&SecretKey::password("jane@example.com"), "hunter2")
            .unwrap();
        let out = format!("{store:?}");
        assert!(!out.contains("hunter2"));
        assert!(!out.contains("jane@example.com"));
        assert!(out.contains(REDACTED));
    }

    #[test]
    fn secret_debug_does_not_leak() {
        let secret = SecretString::new("hunter2");
        let out = format!("{secret:?}");
        assert!(!out.contains("hunter2"));
        assert!(out.contains(REDACTED));
        assert_eq!(secret.expose(), "hunter2");
    }

    #[test]
    fn unavailable_store_refuses_put() {
        let store = UnavailableStore;
        let err = store
            .put(&SecretKey::password("john"), "hunter2")
            .unwrap_err();
        assert!(err.is_unavailable());
        assert!(!err.to_string().contains("hunter2"));
        assert!(err.to_string().contains("mailbox file"));
    }

    #[test]
    fn empty_account_or_secret_is_invalid() {
        let store = MemorySecretStore::new();
        assert!(matches!(
            store.put(&SecretKey::password(""), "x"),
            Err(SecretError::Invalid { .. })
        ));
        assert!(matches!(
            store.put(&SecretKey::password("john"), ""),
            Err(SecretError::Invalid { .. })
        ));
    }

    #[test]
    fn sqlite_has_no_password_column() {
        let db = feathermail_db::Database::memory().unwrap();
        let names = db.table_names().unwrap();
        let banned = [
            "password",
            "token",
            "secret",
            "oauth",
            "access_token",
            "refresh_token",
        ];
        let mut cols = HashSet::new();
        for table in &names {
            let mut stmt = db
                .conn()
                .prepare(&format!("PRAGMA table_info({table})"))
                .unwrap();
            let table_cols: Vec<String> = stmt
                .query_map([], |row| row.get(1))
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            for col in table_cols {
                cols.insert(format!("{table}.{col}"));
                let lower = col.to_ascii_lowercase();
                assert!(
                    !banned.contains(&lower.as_str()),
                    "secret column {table}.{col} (D14)"
                );
            }
        }
        assert!(cols.iter().any(|c| c.starts_with("accounts.")));
        assert!(!cols.iter().any(|c| c.ends_with(".password")));
    }

    #[test]
    fn libsecret_roundtrip_when_service_present() {
        let Ok(store) = LibsecretStore::connect() else {
            return;
        };
        let id = format!(
            "t010-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let key = SecretKey::password(&id);
        let result = (|| {
            store.put(&key, "hunter2")?;
            let got = store.get(&key)?.ok_or_else(|| SecretError::Backend {
                message: "put did not roundtrip".into(),
            })?;
            if got.expose() != "hunter2" {
                return Err(SecretError::Backend {
                    message: "put/get mismatch".into(),
                });
            }
            store.delete(&key)?;
            if store.get(&key)?.is_some() {
                return Err(SecretError::Backend {
                    message: "delete left a secret".into(),
                });
            }
            Ok(())
        })();
        let _ = store.delete(&key);
        result.unwrap();
    }
}
