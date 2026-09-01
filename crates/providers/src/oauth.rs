//! Google OAuth 2.0 loopback + PKCE (T-019, D19). Tokens never go to sqlite.
//!
//! `Pkce`, `TokenSet`, `AuthSession`, `HttpForm`, the json helpers and the
//! token-endpoint plumbing are provider-agnostic and reused as-is by
//! Microsoft's mirror in `microsoft.rs` (T-020) via `pub(crate)` seams.

use std::fmt;
use std::fs::File;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use feathermail_core::{
    AccountId, ApplyError, ConnectError, Core, CoreError, ErrorCode, Reauthenticate,
};
use feathermail_security::{SecretKey, SecretStore};
use sha2::{Digest, Sha256};

use crate::apply::{connect_to_apply, ImapMailProvider};
use crate::autoconfig::LiveHttp;
use crate::session::{ImapAuth, ImapSession};

pub const GOOGLE_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
pub const GOOGLE_USERINFO_URL: &str = "https://openidconnect.googleapis.com/v1/userinfo";
pub const GMAIL_SCOPE: &str = "https://mail.google.com/";
/// Google identity scopes let the native client bind the selected Google
/// account to the IMAP account without a redundant email field in the UI.
pub const GOOGLE_SCOPES: &str = "openid email https://mail.google.com/";

pub(crate) const TOKEN_TIMEOUT: Duration = Duration::from_secs(15);
pub(crate) const LOOPBACK_TIMEOUT: Duration = Duration::from_secs(180);
const BUILD_CLIENT_ID: Option<&str> = option_env!("FEATHERMAIL_GOOGLE_CLIENT_ID");
const BUILD_CLIENT_SECRET: Option<&str> = option_env!("FEATHERMAIL_GOOGLE_CLIENT_SECRET");

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoogleClientConfig {
    pub client_id: String,
    pub client_secret: String,
}

impl GoogleClientConfig {
    pub fn load() -> Result<Self, OauthError> {
        Self::load_from(
            std::env::var("FEATHERMAIL_GOOGLE_CLIENT_ID").ok(),
            std::env::var("FEATHERMAIL_GOOGLE_CLIENT_SECRET").ok(),
            &default_config_path(),
            BUILD_CLIENT_ID,
            BUILD_CLIENT_SECRET,
        )
    }

    pub fn load_from(
        env_id: Option<String>,
        env_secret: Option<String>,
        path: &Path,
        build_id: Option<&str>,
        build_secret: Option<&str>,
    ) -> Result<Self, OauthError> {
        let (file_id, file_secret) = read_toml_section(path, "google").unwrap_or_default();
        let client_id = first_real(&[env_id.as_deref(), file_id.as_deref(), build_id]);
        let client_secret =
            first_real(&[env_secret.as_deref(), file_secret.as_deref(), build_secret])
                .unwrap_or_default();
        match client_id {
            Some(client_id) => Ok(Self {
                client_id,
                client_secret,
            }),
            None => Err(OauthError::not_configured()),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    pub fn generate() -> Result<Self, OauthError> {
        let mut raw = [0u8; 32];
        urandom(&mut raw)?;
        let verifier = URL_SAFE_NO_PAD.encode(raw);
        Ok(Self::from_verifier(verifier))
    }

    pub fn from_verifier(verifier: String) -> Self {
        let digest = Sha256::digest(verifier.as_bytes());
        let challenge = URL_SAFE_NO_PAD.encode(digest);
        Self {
            verifier,
            challenge,
        }
    }
}

impl fmt::Debug for Pkce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pkce")
            .field("challenge", &self.challenge)
            .field("verifier", &"[redacted]")
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct TokenSet {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: Option<u64>,
}

impl fmt::Debug for TokenSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenSet")
            .field("access_token", &"[redacted]")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "[redacted]"),
            )
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OauthError {
    NotConfigured {
        message: String,
    },
    Revoked {
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

impl OauthError {
    pub fn not_configured() -> Self {
        Self::not_configured_for("Google")
    }

    /// Same as [`Self::not_configured`] but for any provider (T-020).
    pub fn not_configured_for(provider: &str) -> Self {
        Self::NotConfigured {
            message: format!(
                "{provider} sign-in needs a Desktop client ID. Use Other IMAP with an app password."
            ),
        }
    }

    pub fn revoked(details: impl Into<String>) -> Self {
        Self::Revoked {
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

    pub fn retry(&self) -> bool {
        matches!(self, Self::Network { .. })
    }

    pub fn apply_error(&self) -> ApplyError {
        match self {
            Self::Network { .. } => ApplyError::Network,
            _ => ApplyError::Auth,
        }
    }
}

impl From<OauthError> for ConnectError {
    fn from(err: OauthError) -> Self {
        match err {
            OauthError::NotConfigured { message } | OauthError::Invalid { message, .. } => {
                ConnectError::invalid(message)
            }
            OauthError::Revoked { message, details } => ConnectError::Auth { message, details },
            OauthError::Network { message, details } => ConnectError::Network { message, details },
        }
    }
}

impl From<OauthError> for CoreError {
    fn from(err: OauthError) -> Self {
        ConnectError::from(err).into()
    }
}

pub trait HttpForm {
    fn post_form(&self, url: &str, form: &[(&str, &str)]) -> Result<(u16, String), OauthError>;
}

impl HttpForm for LiveHttp {
    fn post_form(&self, url: &str, form: &[(&str, &str)]) -> Result<(u16, String), OauthError> {
        let agent = ureq::builder().timeout(TOKEN_TIMEOUT).build();
        match agent.post(url).send_form(form) {
            Ok(resp) => {
                let status = resp.status();
                let body = resp
                    .into_string()
                    .map_err(|e| OauthError::network(e.to_string()))?;
                Ok((status, body))
            }
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                Ok((code, body))
            }
            Err(e) => Err(OauthError::network(e.to_string())),
        }
    }
}

pub struct GoogleOauth<H> {
    pub config: GoogleClientConfig,
    pub http: H,
}

impl<H: HttpForm> GoogleOauth<H> {
    pub fn new(config: GoogleClientConfig, http: H) -> Self {
        Self { config, http }
    }

    pub fn authorization_url(&self, redirect_uri: &str, pkce: &Pkce, state: &str) -> String {
        format!(
            "{GOOGLE_AUTH_URL}?client_id={}&redirect_uri={}&response_type=code&scope={}&access_type=offline&prompt=consent&code_challenge={}&code_challenge_method=S256&state={}",
            enc(&self.config.client_id),
            enc(redirect_uri),
            enc(GOOGLE_SCOPES),
            enc(&pkce.challenge),
            enc(state),
        )
    }

    /// Open the system browser and wait on the loopback port (live sign-in).
    pub fn authorize(&self) -> Result<TokenSet, OauthError> {
        let session = self.begin_loopback()?;
        open_browser(&session.authorize_url, "Google")?;
        self.finish(session, LOOPBACK_TIMEOUT)
    }

    pub fn begin_loopback(&self) -> Result<AuthSession, OauthError> {
        let listener =
            TcpListener::bind("127.0.0.1:0").map_err(|e| OauthError::network(e.to_string()))?;
        let port = listener
            .local_addr()
            .map_err(|e| OauthError::network(e.to_string()))?
            .port();
        let redirect_uri = format!("http://127.0.0.1:{port}");
        let pkce = Pkce::generate()?;
        let state = random_state()?;
        let authorize_url = self.authorization_url(&redirect_uri, &pkce, &state);
        Ok(AuthSession::new(
            authorize_url,
            redirect_uri,
            state,
            pkce,
            listener,
            "Google",
        ))
    }

    pub fn finish(&self, session: AuthSession, timeout: Duration) -> Result<TokenSet, OauthError> {
        let code = session.wait_code(timeout)?;
        self.exchange(&session.redirect_uri, &session.pkce, &code)
    }

    pub fn exchange(
        &self,
        redirect_uri: &str,
        pkce: &Pkce,
        code: &str,
    ) -> Result<TokenSet, OauthError> {
        let mut form = vec![
            ("client_id", self.config.client_id.as_str()),
            ("code", code),
            ("code_verifier", pkce.verifier.as_str()),
            ("grant_type", "authorization_code"),
            ("redirect_uri", redirect_uri),
        ];
        if !self.config.client_secret.is_empty() {
            form.push(("client_secret", self.config.client_secret.as_str()));
        }
        token_response(&self.http, GOOGLE_TOKEN_URL, "Google", &form)
    }

    /// Refresh the access token. No browser. Revoked grant → AUTH_REQUIRED, no retry.
    pub fn refresh(&self, refresh_token: &str) -> Result<TokenSet, OauthError> {
        if refresh_token.is_empty() {
            return Err(OauthError::revoked("missing_refresh_token"));
        }
        let mut form = vec![
            ("client_id", self.config.client_id.as_str()),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ];
        if !self.config.client_secret.is_empty() {
            form.push(("client_secret", self.config.client_secret.as_str()));
        }
        token_response(&self.http, GOOGLE_TOKEN_URL, "Google", &form)
    }
}

/// Return the verified email address for the Google account that granted the
/// current access token. This is fetched from the OIDC UserInfo endpoint over
/// HTTPS, rather than trusting a manually typed address or decoding a JWT in
/// the client. The access token never leaves this function or an HTTPS header.
pub fn google_account_email(access_token: &str) -> Result<String, OauthError> {
    let agent = ureq::builder().timeout(TOKEN_TIMEOUT).build();
    let response = agent
        .get(GOOGLE_USERINFO_URL)
        .set("Authorization", &format!("Bearer {access_token}"))
        .call();
    let body = match response {
        Ok(response) => response
            .into_string()
            .map_err(|err| OauthError::network(err.to_string()))?,
        Err(ureq::Error::Status(_, _)) => {
            return Err(OauthError::invalid(
                "Google didn't return a verified email address for the selected account.",
            ));
        }
        Err(err) => return Err(OauthError::network(err.to_string())),
    };
    google_account_email_from_json(&body)
}

/// Fetches a fresh access token from a stored `refresh_token` (T-083).
/// Implemented for [`GoogleOauth`] and `crate::microsoft::MicrosoftOauth`
/// so [`OauthReauth`] stays generic over which provider it is reauthing
/// instead of duplicating itself per provider.
pub trait TokenRefresh {
    fn refresh(&self, refresh_token: &str) -> Result<TokenSet, OauthError>;
}

impl<H: HttpForm> TokenRefresh for GoogleOauth<H> {
    fn refresh(&self, refresh_token: &str) -> Result<TokenSet, OauthError> {
        GoogleOauth::refresh(self, refresh_token)
    }
}

/// Lets a boxed [`TokenRefresh`] (T-083, third review round) satisfy the
/// trait itself, the same forwarding shape `feathermail_security::secret`'s
/// `impl<T: SecretStore + ?Sized> SecretStore for Arc<T>` already uses for
/// `SecretStore` (T-088). `crates/service`'s `ImapProviderFactory` holds
/// its OAuth client behind a `Box<dyn TokenRefresh>` field (object-safe,
/// one non-generic method) so `connect()` does not need a second generic
/// type parameter threaded through it just to pick Google vs. Microsoft --
/// [`OauthReauth`] still needs a plain `O: TokenRefresh` bound to stay
/// generic over *some* concrete client, and this impl is what lets
/// `Box<dyn TokenRefresh>` be that `O`.
impl TokenRefresh for Box<dyn TokenRefresh> {
    fn refresh(&self, refresh_token: &str) -> Result<TokenSet, OauthError> {
        (**self).refresh(refresh_token)
    }
}

/// T-083's one concrete [`Reauthenticate`] impl: an access token that was
/// merely *expired* (not revoked) is exactly the case a `refresh_token`
/// exists to fix. This is deliberately self-contained rather than reusing
/// whatever `Core`/session the provider it is reauthing already had open:
/// it opens its own `Core::open(db_path)` and asks it for the account's
/// `MailboxForm` fresh, then dials a brand new [`ImapSession`] with the
/// newly-issued access token. That mirrors -- without calling into --
/// exactly what `crates/service/src/provider_factory.rs`'s composition
/// root does on an ordinary connect, so there remains exactly one place
/// (`account_connection` + a fresh `ImapSession::connect`) that knows how
/// to turn an account id into a live session, not two that could drift
/// apart over time.
///
/// Only ever called at most once per [`ApplyError::Auth`] -- see
/// [`Reauthenticate`]'s doc comment in `feathermail_core::provider` for
/// why that bound needs no counter here.
pub struct OauthReauth<O, S> {
    account_id: AccountId,
    db_path: PathBuf,
    oauth: O,
    secrets: S,
}

impl<O, S> OauthReauth<O, S> {
    pub fn new(account_id: AccountId, db_path: impl Into<PathBuf>, oauth: O, secrets: S) -> Self {
        Self {
            account_id,
            db_path: db_path.into(),
            oauth,
            secrets,
        }
    }
}

impl<O: TokenRefresh, S: SecretStore> Reauthenticate<ImapMailProvider<Core>> for OauthReauth<O, S> {
    fn reauthenticate(&mut self) -> Result<ImapMailProvider<Core>, ApplyError> {
        // No refresh token saved at all -- nothing to try; this is the
        // "revoked/never granted" shape, not "expired", so it is terminal
        // exactly like a genuinely revoked grant would be.
        let refresh_key = SecretKey::oauth_refresh(self.account_id.as_str());
        let refresh_token = self
            .secrets
            .get(&refresh_key)
            .map_err(|_| ApplyError::Auth)?
            .ok_or(ApplyError::Auth)?;

        // `OauthError::apply_error()` already draws exactly the line this
        // wrapper needs: `Network` (token endpoint unreachable) stays
        // retryable through D32's existing backoff; anything else
        // (`Revoked`/`Invalid`/`NotConfigured`) is `Auth` and terminal.
        let tokens = self
            .oauth
            .refresh(refresh_token.expose())
            .map_err(|err| err.apply_error())?;

        // Persist the freshly-issued access token *before* dialing back
        // out with it: if the process crashes between here and the
        // reconnect below, the keyring is left holding the newer token
        // (still good for the next attempt) rather than the one that just
        // got rejected.
        //
        // A provider that rotates refresh tokens (Microsoft) hands back a
        // new one on every exchange and lets the old one expire with the
        // original grant; dropping it here would force a browser sign-in
        // on a continuously used account. A response without one (Google)
        // must leave the stored token alone -- overwriting it with an
        // empty string would turn a working account into AUTH_REQUIRED.
        if let Some(rotated) = tokens.refresh_token.as_deref().filter(|s| !s.is_empty()) {
            self.secrets
                .put(&refresh_key, rotated)
                .map_err(|_| ApplyError::Auth)?;
        }
        let access_key = SecretKey::oauth_access(self.account_id.as_str());
        self.secrets
            .put(&access_key, &tokens.access_token)
            .map_err(|_| ApplyError::Auth)?;

        let locator = Core::open(&self.db_path).map_err(|_| ApplyError::Auth)?;
        let conn = locator
            .account_connection(&self.account_id)
            .map_err(|_| ApplyError::Auth)?;
        let session = ImapSession::connect(&conn.form, ImapAuth::XOauth2(tokens.access_token))
            .map_err(connect_to_apply)?;

        Ok(ImapMailProvider::new(session, locator))
    }
}

pub struct AuthSession {
    pub authorize_url: String,
    pub redirect_uri: String,
    pub state: String,
    pub pkce: Pkce,
    listener: TcpListener,
    provider_name: &'static str,
}

impl AuthSession {
    pub(crate) fn new(
        authorize_url: String,
        redirect_uri: String,
        state: String,
        pkce: Pkce,
        listener: TcpListener,
        provider_name: &'static str,
    ) -> Self {
        Self {
            authorize_url,
            redirect_uri,
            state,
            pkce,
            listener,
            provider_name,
        }
    }

    pub fn wait_code(&self, timeout: Duration) -> Result<String, OauthError> {
        self.listener
            .set_nonblocking(true)
            .map_err(|e| OauthError::network(e.to_string()))?;
        let deadline = Instant::now() + timeout;
        loop {
            match self.listener.accept() {
                Ok((stream, addr)) => {
                    if !addr.ip().is_loopback() {
                        continue;
                    }
                    // Only a request carrying *our* `state` decides this
                    // wait. A browser preconnect, a port scanner, or
                    // another process' `?error=` would otherwise end the
                    // sign-in on the first connection that is not the
                    // redirect we are waiting for.
                    match read_code(stream, &self.state, self.provider_name) {
                        LoopbackOutcome::Code(code) => return Ok(code),
                        LoopbackOutcome::Failed(err) => return Err(err),
                        LoopbackOutcome::Stray => continue,
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Err(OauthError::invalid(format!(
                            "{} sign-in timed out.",
                            self.provider_name
                        )));
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => return Err(OauthError::network(e.to_string())),
            }
        }
    }
}

pub fn open_browser(url: &str, provider_name: &str) -> Result<(), OauthError> {
    let status = std::process::Command::new("xdg-open")
        .arg(url)
        .status()
        .map_err(|e| OauthError::network(e.to_string()))?;
    if status.success() {
        Ok(())
    } else {
        Err(OauthError::invalid(format!(
            "Couldn't open a browser for {provider_name} sign-in."
        )))
    }
}

pub fn sasl_xoauth2(email: &str, access_token: &str) -> Vec<u8> {
    let mut sasl = Vec::with_capacity(32 + email.len() + access_token.len());
    sasl.extend_from_slice(b"user=");
    sasl.extend_from_slice(email.as_bytes());
    sasl.push(0x01);
    sasl.extend_from_slice(b"auth=Bearer ");
    sasl.extend_from_slice(access_token.as_bytes());
    sasl.push(0x01);
    sasl.push(0x01);
    let encoded = STANDARD.encode(&sasl).into_bytes();
    sasl.fill(0);
    encoded
}

pub(crate) fn token_response(
    http: &impl HttpForm,
    token_url: &str,
    provider_name: &str,
    form: &[(&str, &str)],
) -> Result<TokenSet, OauthError> {
    let (status, body) = http.post_form(token_url, form)?;
    if status == 200 {
        return parse_token_json(&body, provider_name);
    }
    let error = json_string(&body, "error").unwrap_or_default();
    match error.as_str() {
        // A refresh grant that Google no longer accepts means the user must
        // sign in again. This is deliberately the only token-endpoint
        // family that becomes AUTH_REQUIRED.
        "invalid_grant" | "invalid_token" => {
            return Err(OauthError::revoked(error_or_status(&error, status)));
        }
        // `invalid_client` is a build/configuration problem, not a revoked
        // mailbox. Reporting it as "Sign in again" sent users in circles
        // when a Desktop OAuth client ID was missing or mistyped.
        "invalid_client" => {
            return Err(OauthError::invalid(format!(
                "{provider_name} sign-in needs a Desktop client ID."
            )));
        }
        // These are OAuth application/request configuration errors. The
        // standardized error *code* is safe to show (unlike Google's
        // free-form response details) and gives the user a useful next step
        // instead of collapsing every failure into one opaque sentence.
        "invalid_request" => {
            return Err(OauthError::invalid(format!(
                "{provider_name} rejected this sign-in request. Please update Feather Mail and try again."
            )));
        }
        "invalid_scope" => {
            return Err(OauthError::invalid(format!(
                "{provider_name} denied the requested mailbox permission. Check the OAuth consent screen."
            )));
        }
        "unauthorized_client" => {
            return Err(OauthError::invalid(format!(
                "{provider_name} blocked this OAuth client. Check its publishing status and client type."
            )));
        }
        _ => {}
    }
    if (400..500).contains(&status) && status != 429 {
        return Err(OauthError::invalid(format!(
            "{provider_name} sign-in couldn't be completed. Check the OAuth client configuration."
        )));
    }
    Err(OauthError::network(error_or_status(&error, status)))
}

fn parse_token_json(body: &str, provider_name: &str) -> Result<TokenSet, OauthError> {
    let access_token = json_string(body, "access_token").ok_or_else(|| {
        OauthError::invalid(format!("{provider_name} didn't return an access token."))
    })?;
    if access_token.is_empty() {
        return Err(OauthError::invalid(format!(
            "{provider_name} didn't return an access token."
        )));
    }
    Ok(TokenSet {
        access_token,
        refresh_token: json_string(body, "refresh_token").filter(|s| !s.is_empty()),
        expires_in: json_u64(body, "expires_in"),
    })
}

fn json_string(body: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let i = body.find(&needle)?;
    let rest = body[i + needle.len()..]
        .trim_start()
        .strip_prefix(':')?
        .trim_start();
    let rest = rest.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        if c == '"' {
            return Some(out);
        }
        if c == '\\' {
            match chars.next()? {
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                '/' => out.push('/'),
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                other => out.push(other),
            }
        } else {
            out.push(c);
        }
    }
    None
}

fn json_u64(body: &str, key: &str) -> Option<u64> {
    let needle = format!("\"{key}\"");
    let i = body.find(&needle)?;
    let rest = body[i + needle.len()..]
        .trim_start()
        .strip_prefix(':')?
        .trim_start();
    let num: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    num.parse().ok()
}

fn json_bool(body: &str, key: &str) -> Option<bool> {
    let needle = format!("\"{key}\"");
    let i = body.find(&needle)?;
    let rest = body[i + needle.len()..]
        .trim_start()
        .strip_prefix(':')?
        .trim_start();
    if rest.starts_with("true") {
        Some(true)
    } else if rest.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

fn google_account_email_from_json(body: &str) -> Result<String, OauthError> {
    let email = json_string(body, "email").filter(|email| !email.is_empty());
    match (email, json_bool(body, "email_verified")) {
        (Some(email), Some(true)) => Ok(email),
        _ => Err(OauthError::invalid(
            "Google didn't return a verified email address for the selected account.",
        )),
    }
}

fn error_or_status(error: &str, status: u16) -> String {
    if error.is_empty() {
        format!("http {status}")
    } else {
        error.to_string()
    }
}

/// What one accepted loopback connection means for the sign-in.
enum LoopbackOutcome {
    /// The redirect we were waiting for; `state` matched.
    Code(String),
    /// `state` matched and the request terminally ended the sign-in
    /// (the user cancelled, or the provider reported an error).
    Failed(OauthError),
    /// Not our redirect at all -- a preconnect, a port scan, or someone
    /// else's request. Keep waiting for the real one.
    Stray,
}

/// Reads one loopback request and decides whether it is our redirect.
///
/// `state` is checked *before* `error=`: a stray `?error=access_denied`
/// from a local process carries no matching `state`, and must not be able
/// to cancel a sign-in it has nothing to do with.
fn read_code(mut stream: TcpStream, expected_state: &str, provider_name: &str) -> LoopbackOutcome {
    if stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .is_err()
    {
        return LoopbackOutcome::Stray;
    }
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while buf.len() < 8192 {
        match stream.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {
                buf.push(byte[0]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => return LoopbackOutcome::Stray,
        }
    }
    let req = String::from_utf8_lossy(&buf);
    let line = req.lines().next().unwrap_or("");
    let path = line.split_whitespace().nth(1).unwrap_or("");
    let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
    let params = parse_query(query);
    let _ = writeln!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n<!doctype html><title>Feather Mail</title>You can close this window."
    );
    let state = params.get("state").cloned().unwrap_or_default();
    if state != expected_state {
        return LoopbackOutcome::Stray;
    }
    if let Some(err) = params.get("error") {
        if err == "access_denied" {
            return LoopbackOutcome::Failed(OauthError::invalid(format!(
                "{provider_name} sign-in was cancelled."
            )));
        }
        return LoopbackOutcome::Failed(OauthError::invalid(format!(
            "{provider_name} sign-in didn't finish."
        )));
    }
    match params.get("code").cloned().filter(|c| !c.is_empty()) {
        Some(code) => LoopbackOutcome::Code(code),
        None => LoopbackOutcome::Failed(OauthError::invalid(format!(
            "{provider_name} sign-in didn't finish."
        ))),
    }
}

fn parse_query(query: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for part in query.split('&') {
        if part.is_empty() {
            continue;
        }
        let (k, v) = part.split_once('=').unwrap_or((part, ""));
        out.insert(url_decode(k), url_decode(v));
    }
    out
}

fn url_decode(s: &str) -> String {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            // Decode by byte, never by `str` slice: `s` comes from a
            // lossy conversion of whatever a local process wrote to the
            // loopback socket, so `i + 3` can land inside a multi-byte
            // character and slicing there would panic.
            b'%' if i + 2 < bytes.len() => {
                match std::str::from_utf8(&bytes[i + 1..i + 3])
                    .ok()
                    .and_then(|hex| u8::from_str_radix(hex, 16).ok())
                {
                    Some(b) => {
                        out.push(b);
                        i += 3;
                    }
                    None => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub(crate) fn enc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub(crate) fn random_state() -> Result<String, OauthError> {
    let mut raw = [0u8; 16];
    urandom(&mut raw)?;
    Ok(raw.iter().map(|b| format!("{b:02x}")).collect())
}

fn urandom(buf: &mut [u8]) -> Result<(), OauthError> {
    File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(buf))
        .map_err(|e| OauthError::network(e.to_string()))
}

pub(crate) fn default_config_path() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("feathermail/oauth.toml");
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".config/feathermail/oauth.toml")
}

/// Read `[section]` `client_id` / `client_secret` from the shared oauth.toml
/// (D19). Used by both `GoogleClientConfig` and `MicrosoftClientConfig`.
pub(crate) fn read_toml_section(
    path: &Path,
    section: &str,
) -> Option<(Option<String>, Option<String>)> {
    let text = std::fs::read_to_string(path).ok()?;
    let target = format!("[{section}]");
    let mut client_id = None;
    let mut client_secret = None;
    let mut in_section = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            in_section = line.eq_ignore_ascii_case(&target);
            continue;
        }
        if !in_section {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let k = k.trim();
        let v = unquote(v.trim());
        if k == "client_id" {
            client_id = Some(v);
        } else if k == "client_secret" {
            client_secret = Some(v);
        }
    }
    Some((client_id, client_secret))
}

fn unquote(v: &str) -> String {
    let v = v.trim();
    if let Some(inner) = v.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        inner.replace("\\\"", "\"").replace("\\\\", "\\")
    } else {
        v.to_string()
    }
}

pub(crate) fn first_real(candidates: &[Option<&str>]) -> Option<String> {
    candidates.iter().copied().flatten().find_map(|s| {
        let s = s.trim();
        if s.is_empty() || s.starts_with("YOUR_") {
            None
        } else {
            Some(s.to_string())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::thread;

    struct MapHttp {
        status: u16,
        body: String,
        calls: Mutex<Vec<Vec<(String, String)>>>,
    }

    impl MapHttp {
        fn new(status: u16, body: &str) -> Self {
            Self {
                status,
                body: body.into(),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl HttpForm for MapHttp {
        fn post_form(&self, url: &str, form: &[(&str, &str)]) -> Result<(u16, String), OauthError> {
            assert_eq!(url, GOOGLE_TOKEN_URL);
            self.calls.lock().unwrap().push(
                form.iter()
                    .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                    .collect(),
            );
            Ok((self.status, self.body.clone()))
        }
    }

    fn cfg() -> GoogleClientConfig {
        GoogleClientConfig {
            client_id: "id.apps.googleusercontent.com".into(),
            client_secret: "secret".into(),
        }
    }

    #[test]
    fn pkce_s256_matches_rfc7636_example() {
        let pkce = Pkce::from_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".into());
        assert_eq!(
            pkce.challenge,
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert!(!format!("{pkce:?}").contains("dBjftJeZ4CVP"));
    }

    #[test]
    fn authorization_url_has_pkce_and_gmail_scope() {
        let oauth = GoogleOauth::new(cfg(), MapHttp::new(200, "{}"));
        let pkce = Pkce::from_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".into());
        let url = oauth.authorization_url("http://127.0.0.1:8765", &pkce, "abc");
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("code_challenge=E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"));
        assert!(url.contains(&enc(GMAIL_SCOPE)));
        assert!(url.contains("access_type=offline"));
        assert!(!url.contains("secret"));
    }

    #[test]
    fn refresh_without_ui() {
        let http = MapHttp::new(
            200,
            r#"{"access_token":"ya29.new","expires_in":3600,"token_type":"Bearer"}"#,
        );
        let oauth = GoogleOauth::new(cfg(), http);
        let tokens = oauth.refresh("rt-keep").unwrap();
        assert_eq!(tokens.access_token, "ya29.new");
        assert!(tokens.refresh_token.is_none());
        assert_eq!(tokens.expires_in, Some(3600));
        let calls = oauth.http.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(calls[0]
            .iter()
            .any(|(k, v)| k == "grant_type" && v == "refresh_token"));
        assert!(calls[0]
            .iter()
            .any(|(k, v)| k == "refresh_token" && v == "rt-keep"));
        assert!(!format!("{tokens:?}").contains("ya29.new"));
    }

    #[test]
    fn revoked_refresh_is_auth_not_retry() {
        let http = MapHttp::new(400, r#"{"error":"invalid_grant"}"#);
        let oauth = GoogleOauth::new(cfg(), http);
        let err = oauth.refresh("rt-revoked").unwrap_err();
        assert!(!err.retry());
        match &err {
            OauthError::Revoked { message, details } => {
                assert_eq!(message, "Sign in again to continue.");
                assert_eq!(details.as_deref(), Some("invalid_grant"));
                assert!(!message.to_ascii_lowercase().contains("oauth"));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(oauth.http.calls.lock().unwrap().len(), 1);
        assert_eq!(err.apply_error(), ApplyError::Auth);
        assert!(!err.apply_error().retry());
        let core: CoreError = err.into();
        assert_eq!(core.code, ErrorCode::AuthRequired);
    }

    #[test]
    fn exchange_sends_pkce_verifier() {
        let http = MapHttp::new(
            200,
            r#"{"access_token":"at","refresh_token":"rt","expires_in":10}"#,
        );
        let oauth = GoogleOauth::new(cfg(), http);
        let pkce = Pkce::from_verifier("verifier-value".into());
        let tokens = oauth
            .exchange("http://127.0.0.1:1", &pkce, "code-1")
            .unwrap();
        assert_eq!(tokens.refresh_token.as_deref(), Some("rt"));
        let calls = oauth.http.calls.lock().unwrap();
        assert!(calls[0]
            .iter()
            .any(|(k, v)| k == "code_verifier" && v == "verifier-value"));
        assert!(calls[0]
            .iter()
            .any(|(k, v)| k == "grant_type" && v == "authorization_code"));
    }

    #[test]
    fn public_desktop_client_does_not_send_a_secret() {
        let http = MapHttp::new(
            200,
            r#"{"access_token":"at","refresh_token":"rt","expires_in":10}"#,
        );
        let oauth = GoogleOauth::new(
            GoogleClientConfig {
                client_id: "public.apps.googleusercontent.com".into(),
                client_secret: String::new(),
            },
            http,
        );
        let _ = oauth.refresh("rt").unwrap();
        let calls = oauth.http.calls.lock().unwrap();
        assert!(!calls[0].iter().any(|(key, _)| key == "client_secret"));
    }

    #[test]
    fn invalid_client_is_a_configuration_error_not_reauth() {
        let http = MapHttp::new(401, r#"{"error":"invalid_client"}"#);
        let err = GoogleOauth::new(cfg(), http)
            .refresh("rt")
            .expect_err("invalid_client must fail");
        match err {
            OauthError::Invalid { message, .. } => {
                assert_eq!(message, "Google sign-in needs a Desktop client ID.");
            }
            other => panic!("expected configuration error, got {other:?}"),
        }
    }

    #[test]
    fn other_token_endpoint_client_errors_do_not_retry_as_network() {
        let http = MapHttp::new(400, r#"{"error":"unsupported_grant_type"}"#);
        let err = GoogleOauth::new(cfg(), http)
            .refresh("rt")
            .expect_err("a client error must fail");
        assert!(matches!(err, OauthError::Invalid { .. }));
        assert!(!err.retry());
    }

    #[test]
    fn token_request_errors_keep_a_safe_actionable_class() {
        for (code, expected) in [
            (
                "invalid_request",
                "Google rejected this sign-in request. Please update Feather Mail and try again.",
            ),
            (
                "invalid_scope",
                "Google denied the requested mailbox permission. Check the OAuth consent screen.",
            ),
            (
                "unauthorized_client",
                "Google blocked this OAuth client. Check its publishing status and client type.",
            ),
        ] {
            let http = MapHttp::new(400, &format!(r#"{{"error":"{code}"}}"#));
            let err = GoogleOauth::new(cfg(), http)
                .refresh("rt")
                .expect_err("OAuth client error must not retry as a network failure");
            match err {
                OauthError::Invalid { message, .. } => assert_eq!(message, expected),
                other => panic!("expected invalid OAuth error, got {other:?}"),
            }
        }
    }

    #[test]
    fn google_identity_requires_a_verified_email() {
        assert_eq!(
            google_account_email_from_json(r#"{"email":"you@example.com","email_verified":true}"#)
                .unwrap(),
            "you@example.com"
        );
        for body in [
            r#"{"email":"you@example.com","email_verified":false}"#,
            r#"{"email":"you@example.com"}"#,
            r#"{"email_verified":true}"#,
        ] {
            let err = google_account_email_from_json(body).unwrap_err();
            match err {
                OauthError::Invalid { message, .. } => assert_eq!(
                    message,
                    "Google didn't return a verified email address for the selected account."
                ),
                other => panic!("expected invalid Google identity error, got {other:?}"),
            }
        }
    }

    #[test]
    fn load_skips_placeholder_and_prefers_env() {
        let dir = tempfile_path();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("oauth.toml");
        std::fs::write(
            &path,
            "[google]\nclient_id = \"YOUR_ID.apps.googleusercontent.com\"\nclient_secret = \"YOUR_SECRET\"\n",
        )
        .unwrap();
        let err = GoogleClientConfig::load_from(None, None, &path, None, None).unwrap_err();
        assert!(matches!(err, OauthError::NotConfigured { .. }));
        if let OauthError::NotConfigured { message } = &err {
            assert!(message.contains("Other IMAP"));
            assert!(message.contains("app password"));
            assert!(!message.contains("set up on this computer"));
        }
        let cfg = GoogleClientConfig::load_from(
            Some("env-id.apps.googleusercontent.com".into()),
            Some("env-secret".into()),
            &path,
            Some("build-id"),
            None,
        )
        .unwrap();
        assert_eq!(cfg.client_id, "env-id.apps.googleusercontent.com");
        assert_eq!(cfg.client_secret, "env-secret");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn loopback_captures_code() {
        let oauth = GoogleOauth::new(cfg(), MapHttp::new(200, "{}"));
        let session = oauth.begin_loopback().unwrap();
        let url = session.redirect_uri.clone();
        let state = session.state.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(40));
            let _ = ureq::get(&format!("{url}/?code=the-code&state={state}"))
                .timeout(Duration::from_secs(2))
                .call();
        });
        let code = session.wait_code(Duration::from_secs(2)).unwrap();
        assert_eq!(code, "the-code");
    }

    #[test]
    fn url_decode_survives_percent_before_a_multibyte_char() {
        // `read_code` runs the request through `String::from_utf8_lossy`,
        // so an invalid byte right after `%` becomes U+FFFD (3 bytes).
        let lossy = String::from_utf8_lossy(b"code=%\xff\xfe").into_owned();
        let decoded = url_decode(&lossy);
        assert!(
            decoded.starts_with("code=%"),
            "a stray `%` must stay literal, got {decoded:?}"
        );
        assert_eq!(url_decode("a=%E2%82%ACx"), "a=\u{20ac}x");
        // A `%` right before a multi-byte character, and truncated tails.
        assert_eq!(url_decode("%\u{20ac}"), "%\u{20ac}");
        assert_eq!(url_decode("%"), "%");
        assert_eq!(url_decode("%A"), "%A");
        assert_eq!(url_decode("%z9"), "%z9");
        assert_eq!(url_decode("%\u{e9}9"), "%\u{e9}9");
    }

    #[test]
    fn stray_connection_does_not_end_the_wait() {
        let oauth = GoogleOauth::new(cfg(), MapHttp::new(200, "{}"));
        let session = oauth.begin_loopback().unwrap();
        let url = session.redirect_uri.clone();
        let state = session.state.clone();
        let addr = url.trim_start_matches("http://").to_string();
        thread::spawn(move || {
            // A browser preconnect: opens the socket, sends nothing.
            thread::sleep(Duration::from_millis(40));
            let stray = std::net::TcpStream::connect(&addr).unwrap();
            drop(stray);
            thread::sleep(Duration::from_millis(60));
            let _ = ureq::get(&format!("{url}/?code=the-code&state={state}"))
                .timeout(Duration::from_secs(2))
                .call();
        });
        let code = session.wait_code(Duration::from_secs(3)).unwrap();
        assert_eq!(code, "the-code");
    }

    #[test]
    fn foreign_error_without_our_state_does_not_cancel_the_wait() {
        let oauth = GoogleOauth::new(cfg(), MapHttp::new(200, "{}"));
        let session = oauth.begin_loopback().unwrap();
        let url = session.redirect_uri.clone();
        let state = session.state.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(40));
            let _ = ureq::get(&format!("{url}/?error=access_denied"))
                .timeout(Duration::from_secs(2))
                .call();
            thread::sleep(Duration::from_millis(60));
            let _ = ureq::get(&format!("{url}/?code=the-code&state={state}"))
                .timeout(Duration::from_secs(2))
                .call();
        });
        let code = session.wait_code(Duration::from_secs(3)).unwrap();
        assert_eq!(code, "the-code");
    }

    #[test]
    fn a_real_cancellation_ends_the_wait_without_waiting_for_the_deadline() {
        let oauth = GoogleOauth::new(cfg(), MapHttp::new(200, "{}"));
        let session = oauth.begin_loopback().unwrap();
        let url = session.redirect_uri.clone();
        let state = session.state.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(40));
            let _ = ureq::get(&format!("{url}/?error=access_denied&state={state}"))
                .timeout(Duration::from_secs(2))
                .call();
        });
        let started = Instant::now();
        let err = session.wait_code(Duration::from_secs(30)).unwrap_err();
        assert!(
            matches!(&err, OauthError::Invalid { message, .. } if message.contains("cancelled")),
            "a cancellation carrying our state is terminal, got {err:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the wizard must not hang until the deadline after a real cancellation"
        );
    }

    #[test]
    fn sasl_xoauth2_matches_gmail_spec() {
        let mut raw = sasl_xoauth2("user@gmail.com", "ya29.v");
        let decoded = STANDARD.decode(&raw).unwrap();
        raw.fill(0);
        assert_eq!(
            decoded,
            b"user=user@gmail.com\x01auth=Bearer ya29.v\x01\x01"
        );
    }

    // --- OauthReauth (T-083) ---------------------------------------

    struct OkConnector;
    impl feathermail_core::MailConnector for OkConnector {
        fn probe(
            &self,
            _form: &feathermail_core::MailboxForm,
            _password: &str,
        ) -> Result<feathermail_core::ConnectOk, ConnectError> {
            Ok(feathermail_core::ConnectOk {
                capabilities: Vec::new(),
            })
        }
    }

    /// Saves one "generic" account (T-083's `OauthReauth` doesn't care what
    /// `provider` string is on the row -- it only reads connection fields
    /// through `account_connection` and secrets through the keyring, same
    /// as `provider_factory.rs` does for a real Gmail/Microsoft account)
    /// pointed at `imap_port` on localhost, and returns its id.
    fn seed_account(db_path: &Path, imap_port: u16) -> AccountId {
        let form = crate::xoauth2::plaintext_form("reauth@example.com", imap_port, 1);
        let mut core = Core::open(db_path).unwrap();
        core.add_account(&form, "unused-password", &OkConnector)
            .unwrap()
    }

    #[test]
    fn oauth_reauth_refreshes_and_reconnects_with_the_new_token() {
        let db_path = tempfile_path();
        let imap_port = crate::xoauth2::fake_server::spawn_imap("fresh-access-token");
        let account_id = seed_account(&db_path, imap_port);

        let secrets = feathermail_security::MemorySecretStore::new();
        secrets
            .put(&SecretKey::oauth_refresh(account_id.as_str()), "rt-good")
            .unwrap();

        let http = MapHttp::new(
            200,
            r#"{"access_token":"fresh-access-token","expires_in":3600}"#,
        );
        let oauth = GoogleOauth::new(cfg(), http);
        let mut reauth = OauthReauth::new(account_id.clone(), db_path.clone(), oauth, secrets);

        // A working `ImapMailProvider` only comes back if the whole chain
        // ran for real: HTTP refresh call -> new access token -> a brand
        // new `ImapSession::connect` that actually authenticated against
        // the fake server's XOAUTH2 handler with *that* token.
        let _provider = reauth.reauthenticate().expect("reauth should succeed");

        assert_eq!(reauth.oauth.http.calls.lock().unwrap().len(), 1);
        let stored_access = reauth
            .secrets
            .get(&SecretKey::oauth_access(account_id.as_str()))
            .unwrap()
            .unwrap();
        assert_eq!(
            stored_access.expose(),
            "fresh-access-token",
            "the new access token must be persisted to the keyring, not just used once"
        );
        let stored_refresh = reauth
            .secrets
            .get(&SecretKey::oauth_refresh(account_id.as_str()))
            .unwrap()
            .unwrap();
        assert_eq!(
            stored_refresh.expose(),
            "rt-good",
            "a response without a refresh token (Google) must leave the stored one alone"
        );

        let _ = std::fs::remove_file(&db_path);
    }

    #[test]
    fn rotated_refresh_token_is_persisted() {
        let db_path = tempfile_path();
        let imap_port = crate::xoauth2::fake_server::spawn_imap("fresh-access-token");
        let account_id = seed_account(&db_path, imap_port);

        let secrets = feathermail_security::MemorySecretStore::new();
        secrets
            .put(&SecretKey::oauth_refresh(account_id.as_str()), "rt1")
            .unwrap();

        // Microsoft rotates the refresh token on every refresh; the old
        // one keeps only the original grant's lifetime.
        let http = MapHttp::new(
            200,
            r#"{"access_token":"fresh-access-token","refresh_token":"rt2","expires_in":3600}"#,
        );
        let oauth = GoogleOauth::new(cfg(), http);
        let mut reauth = OauthReauth::new(account_id.clone(), db_path.clone(), oauth, secrets);

        let _provider = reauth.reauthenticate().expect("reauth should succeed");

        let stored_refresh = reauth
            .secrets
            .get(&SecretKey::oauth_refresh(account_id.as_str()))
            .unwrap()
            .unwrap();
        assert_eq!(
            stored_refresh.expose(),
            "rt2",
            "a rotated refresh token must replace the stored one"
        );

        let _ = std::fs::remove_file(&db_path);
    }

    #[test]
    fn oauth_reauth_is_terminal_when_the_server_still_rejects_the_new_token() {
        // Models a genuinely revoked grant that nonetheless still yields a
        // token from the refresh endpoint in some edge case (or a scope
        // mismatch): the *server* is the final word, and rejects even the
        // freshly-issued token. This must come back `Auth`, not hang and
        // not retry -- `reauthenticate()` dials exactly once.
        let db_path = tempfile_path();
        let imap_port = crate::xoauth2::fake_server::spawn_imap("some-other-token-entirely");
        let account_id = seed_account(&db_path, imap_port);

        let secrets = feathermail_security::MemorySecretStore::new();
        secrets
            .put(&SecretKey::oauth_refresh(account_id.as_str()), "rt-good")
            .unwrap();

        let http = MapHttp::new(
            200,
            r#"{"access_token":"fresh-access-token","expires_in":3600}"#,
        );
        let oauth = GoogleOauth::new(cfg(), http);
        let mut reauth = OauthReauth::new(account_id, db_path.clone(), oauth, secrets);

        let err = reauth.reauthenticate().err().unwrap();
        assert_eq!(err, ApplyError::Auth);
        assert!(!err.retry());

        let _ = std::fs::remove_file(&db_path);
    }

    #[test]
    fn oauth_reauth_is_terminal_with_no_refresh_token_saved() {
        // No refresh token in the keyring at all -- there is nothing to
        // try, so this must fail closed as `Auth` without ever reaching
        // the network (asserted via the HTTP fake's call count).
        let db_path = tempfile_path();
        let account_id = seed_account(&db_path, 1);
        let secrets = feathermail_security::MemorySecretStore::new();
        let http = MapHttp::new(200, r#"{"access_token":"unused"}"#);
        let oauth = GoogleOauth::new(cfg(), http);
        let mut reauth = OauthReauth::new(account_id, db_path.clone(), oauth, secrets);

        let err = reauth.reauthenticate().err().unwrap();

        assert_eq!(err, ApplyError::Auth);
        assert_eq!(reauth.oauth.http.calls.lock().unwrap().len(), 0);

        let _ = std::fs::remove_file(&db_path);
    }

    #[test]
    fn oauth_reauth_refresh_network_failure_is_retryable_not_terminal() {
        // The token endpoint itself is unreachable/5xx -- this must come
        // back as `Network` (D32's existing backoff handles it), never as
        // `Auth` (which would roll the user's mark back for a problem that
        // has nothing to do with their credentials).
        let db_path = tempfile_path();
        let account_id = seed_account(&db_path, 1);
        let secrets = feathermail_security::MemorySecretStore::new();
        secrets
            .put(&SecretKey::oauth_refresh(account_id.as_str()), "rt-good")
            .unwrap();
        let http = MapHttp::new(503, "");
        let oauth = GoogleOauth::new(cfg(), http);
        let mut reauth = OauthReauth::new(account_id, db_path.clone(), oauth, secrets);

        let err = reauth.reauthenticate().err().unwrap();

        assert_eq!(err, ApplyError::Network);
        assert!(err.retry());

        let _ = std::fs::remove_file(&db_path);
    }

    fn tempfile_path() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "feathermail-oauth-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        p
    }
}
