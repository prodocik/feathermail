//! T-094: the one-shot account-provisioning thread -- the door between the
//! "Add account" wizard and `Core::add_account` / `add_gmail_account` /
//! `add_microsoft_account`.
//!
//! Why this exists and is not a worker command: provisioning runs a real
//! network probe (seconds) and, for OAuth, waits on the loopback listener
//! for up to `LOOPBACK_TIMEOUT` (180 s). Neither may run inside the sync
//! worker's `run()` loop, whose `events` callback is also not shareable
//! with a side thread. So one attempt = one short-lived thread, spawned
//! here, with its own `Core::open` handle on the profile database (a WAL
//! handle next to the writer/reader/factory ones -- the same precedent
//! [`crate::provider_factory::ImapProviderFactory`] already sets).
//!
//! Error discipline (D14): the `Err` side handed to the sink is human text
//! only -- `ConnectError`/`CoreError`/`OauthError` all carry a non-secret
//! `message` plus potentially protocol-laden `details`; only `message`
//! ever crosses this boundary, mirroring `worker.rs`'s
//! `connect_error_message`. Passwords and tokens travel inside the thread
//! and into the keyring; they never appear in the `Result`.
//!
//! Secret ordering: probe first, account row second, keyring last. A
//! failed probe leaves no account and no keyring entry; a failed keyring
//! write rolls the account back through `Core::remove_account` (T-021),
//! which also sweeps every secret kind. The reverse order would strand a
//! credential in the keyring under an account id nothing else knows.

use std::path::PathBuf;
use std::sync::Arc;

use feathermail_core::{AccountId, Core, MailboxForm};
use feathermail_providers::{
    google_account_email, GenericImapSmtp, GmailImap, GoogleClientConfig, GoogleOauth, LiveHttp,
    MicrosoftClientConfig, MicrosoftImap, MicrosoftOauth, OauthError, TokenSet,
};
use feathermail_security::{SecretKey, SecretStore};

/// Which OAuth provider a [`ProvisionRequest::Oauth`] attempt signs into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OauthProvider {
    Google,
    Microsoft,
}

/// One account-creation attempt. `Debug` is hand-written and redacted:
/// this value carries a mailbox password (and later, inside the thread,
/// OAuth tokens), so the derive would be a D14 leak waiting for a log
/// statement -- the same shape `MailboxDraft` already uses.
pub enum ProvisionRequest {
    /// T-018 path: generic IMAP/SMTP with a LOGIN password.
    Password { form: MailboxForm, password: String },
    /// T-019/T-020 path: loopback + PKCE sign-in, then XOAUTH2 probe.
    /// Google obtains the selected account's verified email through OIDC;
    /// Microsoft still asks for it until its own identity path is added.
    Oauth {
        provider: OauthProvider,
        email: Option<String>,
    },
}

impl std::fmt::Debug for ProvisionRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Password { form, .. } => f
                .debug_struct("Password")
                .field("email", &form.email)
                .field("password", &"[redacted]")
                .finish(),
            Self::Oauth { provider, email } => f
                .debug_struct("Oauth")
                .field("provider", provider)
                .field("email", email)
                .finish(),
        }
    }
}

/// Spawn the one-shot provisioning thread. Returns immediately (D11: the
/// probe and the OAuth wait must never run on the caller's thread, which
/// is the GTK thread for every caller today). Exactly one `sink` call
/// happens per spawn, from the spawned thread.
pub fn spawn_provision(
    db_path: PathBuf,
    secrets: Arc<dyn SecretStore>,
    request: ProvisionRequest,
    sink: impl FnOnce(Result<String, String>) + Send + 'static,
) {
    std::thread::spawn(move || {
        sink(provision(db_path, secrets, request));
    });
}

fn provision(
    db_path: PathBuf,
    secrets: Arc<dyn SecretStore>,
    request: ProvisionRequest,
) -> Result<String, String> {
    let mut core = Core::open(&db_path).map_err(|e| e.message)?;
    match request {
        ProvisionRequest::Password { form, password } => {
            provision_password(&mut core, &secrets, &form, &password)
        }
        ProvisionRequest::Oauth { provider, email } => {
            provision_oauth(&mut core, &secrets, provider, email.as_deref())
        }
    }
}

fn provision_password(
    core: &mut Core,
    secrets: &Arc<dyn SecretStore>,
    form: &MailboxForm,
    password: &str,
) -> Result<String, String> {
    let id = core
        .add_account(form, password, &GenericImapSmtp)
        .map_err(|e| e.message)?;
    if let Err(err) = secrets.put(&SecretKey::password(id.as_str()), password) {
        rollback(core, &id, secrets);
        return Err(err.to_string());
    }
    Ok(id.as_str().to_string())
}

fn provision_oauth(
    core: &mut Core,
    secrets: &Arc<dyn SecretStore>,
    provider: OauthProvider,
    email: Option<&str>,
) -> Result<String, String> {
    let tokens = oauth_sign_in(provider)?;
    let email = match provider {
        OauthProvider::Google => {
            google_account_email(&tokens.access_token).map_err(oauth_error_message)?
        }
        OauthProvider::Microsoft => email
            .filter(|email| !email.is_empty())
            .ok_or_else(|| "Enter an email address.".to_string())?
            .to_string(),
    };
    provision_oauth_tokens(core, secrets, provider, &email, tokens)
}

/// Everything after the browser sign-in: refresh-token gate, XOAUTH2
/// probe via `Core::add_*_account`, keyring writes, rollback. Split from
/// [`provision_oauth`] so the gate and its error text are testable
/// without a real OAuth round-trip (the probe itself still needs the
/// provider's real hosts -- no offline double exists for Gmail/Outlook).
fn provision_oauth_tokens(
    core: &mut Core,
    secrets: &Arc<dyn SecretStore>,
    provider: OauthProvider,
    email: &str,
    tokens: TokenSet,
) -> Result<String, String> {
    let access = tokens.access_token.clone();
    let refresh = tokens.refresh_token.clone().ok_or_else(|| {
        format!(
            "{} didn't return a refresh token, so the account would stop working \
             after an hour. Nothing was saved -- try signing in again.",
            provider_name(provider)
        )
    })?;
    let id = match provider {
        OauthProvider::Google => core
            .add_gmail_account(email, &access, &GmailImap)
            .map_err(|e| e.message)?,
        OauthProvider::Microsoft => core
            .add_microsoft_account(email, &access, &MicrosoftImap)
            .map_err(|e| e.message)?,
    };
    let saved = secrets
        .put(&SecretKey::oauth_access(id.as_str()), &access)
        .and_then(|()| secrets.put(&SecretKey::oauth_refresh(id.as_str()), &refresh));
    if let Err(err) = saved {
        // T-021's `remove_account` deletes every secret kind, so the
        // access token written by a partial success above is swept too.
        rollback(core, &id, secrets);
        return Err(err.to_string());
    }
    Ok(id.as_str().to_string())
}

fn oauth_sign_in(provider: OauthProvider) -> Result<TokenSet, String> {
    match provider {
        OauthProvider::Google => {
            let config = GoogleClientConfig::load().map_err(oauth_error_message)?;
            GoogleOauth::new(config, LiveHttp)
                .authorize()
                .map_err(oauth_error_message)
        }
        OauthProvider::Microsoft => {
            let config = MicrosoftClientConfig::load().map_err(oauth_error_message)?;
            MicrosoftOauth::new(config, LiveHttp)
                .authorize()
                .map_err(oauth_error_message)
        }
    }
}

fn provider_name(provider: OauthProvider) -> &'static str {
    match provider {
        OauthProvider::Google => "Google",
        OauthProvider::Microsoft => "Microsoft",
    }
}

/// D14: only the human `message` crosses to the UI -- `details` may carry
/// protocol text the server echoed back. Same boundary as `worker.rs`'s
/// `connect_error_message`.
fn oauth_error_message(err: OauthError) -> String {
    match err {
        OauthError::NotConfigured { message }
        | OauthError::Revoked { message, .. }
        | OauthError::Network { message, .. }
        | OauthError::Invalid { message, .. } => message,
    }
}

fn rollback(core: &mut Core, id: &AccountId, secrets: &Arc<dyn SecretStore>) {
    // Best effort: the account row must not survive a keyring failure, and
    // a rollback failure has no better channel here than the error the
    // caller is already about to report.
    let _ = core.remove_account(id, secrets);
}

#[cfg(test)]
mod tests {
    use super::*;
    use feathermail_core::MailSecurity;
    use feathermail_providers::test_support::spawn_fake_imap_server;
    use feathermail_security::{MemorySecretStore, SecretError};
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::Barrier;

    struct AlwaysFailStore;

    impl SecretStore for AlwaysFailStore {
        fn put(&self, _key: &SecretKey, _secret: &str) -> Result<(), SecretError> {
            Err(SecretError::Backend {
                message: "test keyring is down".into(),
            })
        }
        fn get(
            &self,
            _key: &SecretKey,
        ) -> Result<Option<feathermail_security::SecretString>, SecretError> {
            Ok(None)
        }
        fn delete(&self, _key: &SecretKey) -> Result<(), SecretError> {
            Ok(())
        }
    }

    /// The smallest SMTP double `GenericImapSmtp`'s probe accepts: greeting
    /// plus 250 to everything (NOOP is all an unauthenticated probe needs).
    fn spawn_smtp_ok() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            while let Ok((stream, _)) = listener.accept() {
                let mut writer = stream.try_clone().unwrap();
                let mut reader = BufReader::new(stream);
                if write!(writer, "220 test ESMTP\r\n").is_err() {
                    continue;
                }
                let _ = writer.flush();
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    let upper = line.trim_end().to_ascii_uppercase();
                    if upper.starts_with("QUIT") {
                        let _ = write!(writer, "221 bye\r\n");
                        break;
                    } else if upper.starts_with("EHLO") || upper.starts_with("HELO") {
                        let _ = write!(writer, "250-hello\r\n250 OK\r\n");
                    } else {
                        let _ = write!(writer, "250 OK\r\n");
                    }
                    let _ = writer.flush();
                }
            }
        });
        port
    }

    /// An IMAP double that rejects every LOGIN, echoing `MARKER` in the NO
    /// response (a stand-in for protocol text the wire never sends, like
    /// the password itself -- the surfaced error must not contain it).
    fn spawn_imap_reject() -> u16 {
        const MARKER: &str = "server-says-no-cookie";
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            while let Ok((stream, _)) = listener.accept() {
                let mut writer = stream.try_clone().unwrap();
                let mut reader = BufReader::new(stream);
                if writeln!(writer, "* OK ready").is_err() {
                    continue;
                }
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    let tag = line.split_whitespace().next().unwrap_or("A").to_string();
                    if line.to_ascii_uppercase().contains(" LOGIN ") {
                        let _ = writeln!(writer, "{tag} NO [AUTHENTICATIONFAILED] {MARKER}");
                    } else {
                        let _ = writeln!(writer, "{tag} OK done");
                    }
                    let _ = writer.flush();
                }
            }
        });
        port
    }

    fn form(imap_port: u16, smtp_port: u16) -> MailboxForm {
        MailboxForm {
            email: "you@example.com".into(),
            imap_host: "127.0.0.1".into(),
            imap_port,
            imap_security: MailSecurity::None,
            smtp_host: "127.0.0.1".into(),
            smtp_port,
            smtp_security: MailSecurity::None,
        }
    }

    fn temp_db() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        (dir, path)
    }

    fn list_account_count(db_path: &PathBuf) -> usize {
        let core = Core::open(db_path).unwrap();
        core.list_accounts().unwrap().len()
    }

    #[test]
    fn password_ok_creates_the_account_and_saves_the_password() {
        let (imap, _state) = spawn_fake_imap_server(vec![("INBOX", vec![])], false);
        let smtp = spawn_smtp_ok();
        let (_dir, db) = temp_db();
        let secrets: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::new());

        let id = provision(
            db.clone(),
            Arc::clone(&secrets),
            ProvisionRequest::Password {
                form: form(imap, smtp),
                password: "hunter2".into(),
            },
        )
        .expect("a good LOGIN must provision");

        let core = Core::open(&db).unwrap();
        let accounts = core.list_accounts().unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].id.as_str(), id);
        assert_eq!(accounts[0].email, "you@example.com");
        let saved = secrets
            .get(&SecretKey::password(&id))
            .unwrap()
            .expect("the password must be in the keyring after provisioning");
        assert_eq!(saved.expose(), "hunter2");
    }

    #[test]
    fn wrong_password_leaves_no_account_and_no_secret() {
        let imap = spawn_imap_reject();
        let smtp = spawn_smtp_ok();
        let (_dir, db) = temp_db();
        let secrets: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::new());

        let err = provision(
            db.clone(),
            Arc::clone(&secrets),
            ProvisionRequest::Password {
                form: form(imap, smtp),
                password: "hunter2".into(),
            },
        )
        .expect_err("a rejected LOGIN must not provision");

        assert_eq!(list_account_count(&db), 0, "no account row may survive");
        assert!(
            secrets
                .get(&SecretKey::password("you@example.com"))
                .unwrap()
                .is_none(),
            "no keyring entry may survive a failed probe"
        );
        // D14: the wire sent `A2 LOGIN "you@example.com" "hunter2"`; nothing
        // on the error path may hand the password back to the UI.
        assert!(
            !err.contains("hunter2"),
            "the error must not carry the password: {err}"
        );
    }

    #[test]
    fn keyring_failure_rolls_the_account_back() {
        let (imap, _state) = spawn_fake_imap_server(vec![("INBOX", vec![])], false);
        let smtp = spawn_smtp_ok();
        let (_dir, db) = temp_db();
        let secrets: Arc<dyn SecretStore> = Arc::new(AlwaysFailStore);

        let err = provision(
            db.clone(),
            secrets,
            ProvisionRequest::Password {
                form: form(imap, smtp),
                password: "hunter2".into(),
            },
        )
        .expect_err("a keyring that cannot save must fail the provisioning");

        assert!(err.contains("test keyring is down"), "{err}");
        assert_eq!(
            list_account_count(&db),
            0,
            "the account must be rolled back when its password cannot be saved"
        );
    }

    #[test]
    fn duplicate_email_is_a_human_error() {
        let (imap, _state) = spawn_fake_imap_server(vec![("INBOX", vec![])], false);
        let smtp = spawn_smtp_ok();
        let (_dir, db) = temp_db();
        let secrets: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::new());
        let first = ProvisionRequest::Password {
            form: form(imap, smtp),
            password: "hunter2".into(),
        };
        provision(db.clone(), Arc::clone(&secrets), first).unwrap();

        let second = ProvisionRequest::Password {
            form: form(imap, smtp),
            password: "hunter2".into(),
        };
        let err = provision(db.clone(), secrets, second).expect_err("duplicate must fail");
        assert!(
            err.contains("already on this computer"),
            "the duplicate error must be the human one: {err}"
        );
        assert_eq!(list_account_count(&db), 1);
    }

    /// D11: `spawn_provision` must return to the caller (the GTK thread in
    /// production) immediately, not after the network probe. Proven with a
    /// server that holds the LOGIN response on a barrier until the test
    /// releases it -- if the spawn were secretly synchronous, this test
    /// would deadlock on the receive below and time out the run.
    #[test]
    fn spawn_provision_returns_before_the_probe_finishes() {
        let barrier = Arc::new(Barrier::new(2));
        let server_barrier = Arc::clone(&barrier);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let imap = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                let mut writer = stream.try_clone().unwrap();
                let mut reader = BufReader::new(stream);
                write!(writer, "* OK ready\r\n").unwrap();
                writer.flush().unwrap();
                let mut line = String::new();
                line.clear();
                reader.read_line(&mut line).unwrap();
                server_barrier.wait(); // hold the probe until the test releases it
                loop {
                    let tag = line.split_whitespace().next().unwrap_or("A").to_string();
                    if write!(writer, "{tag} OK done\r\n").is_err() {
                        break;
                    }
                    if writer.flush().is_err() {
                        break;
                    }
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
            }
        });
        let (_dir, db) = temp_db();
        let (tx, rx) = std::sync::mpsc::channel();
        let started = std::time::Instant::now();
        spawn_provision(
            db,
            Arc::new(MemorySecretStore::new()),
            ProvisionRequest::Password {
                form: form(imap, spawn_smtp_ok()),
                password: "hunter2".into(),
            },
            move |result| {
                tx.send(result).unwrap();
            },
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "spawn_provision must return while the probe is still blocked"
        );
        barrier.wait(); // release the server; the probe completes now
        rx.recv_timeout(std::time::Duration::from_secs(30))
            .expect("the sink must be called exactly once")
            .expect("released LOGIN provisions fine");
    }

    #[test]
    fn provision_request_debug_never_shows_the_password() {
        let req = ProvisionRequest::Password {
            form: form(1, 2),
            password: "hunter2".into(),
        };
        let text = format!("{req:?}");
        assert!(!text.contains("hunter2"), "{text}");
        assert!(text.contains("[redacted]"), "{text}");
        assert!(text.contains("you@example.com"), "{text}");
    }

    /// T-094 OAuth gate: a token response without a refresh token must
    /// fail before the account row exists -- an access-only account dies
    /// after an hour and the user would never know why. Offline-testable
    /// because the gate sits before any provider host is contacted.
    #[test]
    fn oauth_without_a_refresh_token_saves_nothing() {
        let (_dir, db) = temp_db();
        let mut core = Core::open(&db).unwrap();
        let secrets: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::new());
        let tokens = TokenSet {
            access_token: "ya29.test".into(),
            refresh_token: None,
            expires_in: Some(3600),
        };
        let err = provision_oauth_tokens(
            &mut core,
            &secrets,
            OauthProvider::Google,
            "you@gmail.com",
            tokens,
        )
        .expect_err("no refresh token must fail the provisioning");
        assert!(err.contains("refresh token"), "{err}");
        assert!(err.contains("Google"), "{err}");
        assert_eq!(
            core.list_accounts().unwrap().len(),
            0,
            "no account row may exist after the refresh-token gate"
        );
    }

    /// T-094 pins on the OAuth arms themselves (the network halves are
    /// not offline-testable): Google must provision through
    /// `add_gmail_account` with a `GoogleClientConfig` sign-in, Microsoft
    /// through `add_microsoft_account` with `MicrosoftClientConfig`, and
    /// both keyring writes must cover access *and* refresh tokens.
    #[test]
    fn oauth_arms_use_their_own_provider_doors() {
        let src = include_str!("provision.rs");
        let body = src.split("mod tests").next().unwrap();
        assert!(body.contains("add_gmail_account"), "Google arm");
        assert!(body.contains("GoogleClientConfig::load"), "Google sign-in");
        assert!(body.contains("add_microsoft_account"), "Microsoft arm");
        assert!(
            body.contains("MicrosoftClientConfig::load"),
            "Microsoft sign-in"
        );
        assert!(
            body.contains("SecretKey::oauth_access") && body.contains("SecretKey::oauth_refresh"),
            "both token kinds must reach the keyring"
        );
        assert!(
            body.contains("rollback(core, &id, secrets)"),
            "a failed keyring write must roll the account back (T-021)"
        );
    }
}
