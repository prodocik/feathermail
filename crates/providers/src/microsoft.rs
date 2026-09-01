//! Microsoft 365 / Outlook.com OAuth 2.0 (public client + PKCE) and IMAP/SMTP
//! XOAUTH2 (T-020, D18–D19).
//!
//! The PKCE/token/loopback plumbing (`Pkce`, `TokenSet`, `AuthSession`,
//! `HttpForm`, `token_response`, ...) lives in `oauth.rs` and is reused
//! as-is — `MicrosoftOauth` only supplies Microsoft's endpoints/scope. The
//! IMAP/SMTP XOAUTH2 wire handshake lives in `xoauth2.rs` and is shared with
//! `GmailImap`; `MicrosoftImap` here is a one-line wrapper around it.

use std::net::TcpListener;
use std::path::Path;
use std::time::Duration;

use feathermail_core::{ConnectError, ConnectOk, MailConnector, MailboxForm};

use crate::oauth::{
    default_config_path, enc, first_real, open_browser, random_state, read_toml_section,
    token_response, AuthSession, HttpForm, OauthError, Pkce, TokenRefresh, TokenSet,
    LOOPBACK_TIMEOUT,
};
use crate::xoauth2::probe_xoauth2;

pub const MICROSOFT_AUTH_URL: &str =
    "https://login.microsoftonline.com/common/oauth2/v2.0/authorize";
pub const MICROSOFT_TOKEN_URL: &str = "https://login.microsoftonline.com/common/oauth2/v2.0/token";
pub const MICROSOFT_SCOPE: &str =
    "https://outlook.office.com/IMAP.AccessAsUser.All https://outlook.office.com/SMTP.Send offline_access";

const BUILD_CLIENT_ID: Option<&str> = option_env!("FEATHERMAIL_MICROSOFT_CLIENT_ID");
const BUILD_CLIENT_SECRET: Option<&str> = option_env!("FEATHERMAIL_MICROSOFT_CLIENT_SECRET");

/// D19: env → `~/.config/feathermail/oauth.toml` (`[microsoft]`) → build-time.
/// Microsoft desktop apps are public clients; `client_secret` stays optional,
/// same as Google's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MicrosoftClientConfig {
    pub client_id: String,
    pub client_secret: String,
}

impl MicrosoftClientConfig {
    pub fn load() -> Result<Self, OauthError> {
        Self::load_from(
            std::env::var("FEATHERMAIL_MICROSOFT_CLIENT_ID").ok(),
            std::env::var("FEATHERMAIL_MICROSOFT_CLIENT_SECRET").ok(),
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
        let (file_id, file_secret) = read_toml_section(path, "microsoft").unwrap_or_default();
        let client_id = first_real(&[env_id.as_deref(), file_id.as_deref(), build_id]);
        let client_secret =
            first_real(&[env_secret.as_deref(), file_secret.as_deref(), build_secret])
                .unwrap_or_default();
        match client_id {
            Some(client_id) => Ok(Self {
                client_id,
                client_secret,
            }),
            None => Err(OauthError::not_configured_for("Microsoft")),
        }
    }
}

/// Mirror of `GoogleOauth`: public client + PKCE S256, loopback redirect.
pub struct MicrosoftOauth<H> {
    pub config: MicrosoftClientConfig,
    pub http: H,
}

impl<H: HttpForm> MicrosoftOauth<H> {
    pub fn new(config: MicrosoftClientConfig, http: H) -> Self {
        Self { config, http }
    }

    pub fn authorization_url(&self, redirect_uri: &str, pkce: &Pkce, state: &str) -> String {
        format!(
            "{MICROSOFT_AUTH_URL}?client_id={}&redirect_uri={}&response_type=code&scope={}&code_challenge={}&code_challenge_method=S256&state={}",
            enc(&self.config.client_id),
            enc(redirect_uri),
            enc(MICROSOFT_SCOPE),
            enc(&pkce.challenge),
            enc(state),
        )
    }

    /// Open the system browser and wait on the loopback port (live sign-in).
    pub fn authorize(&self) -> Result<TokenSet, OauthError> {
        let session = self.begin_loopback()?;
        open_browser(&session.authorize_url, "Microsoft")?;
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
            "Microsoft",
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
        token_response(&self.http, MICROSOFT_TOKEN_URL, "Microsoft", &form)
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
        token_response(&self.http, MICROSOFT_TOKEN_URL, "Microsoft", &form)
    }
}

/// T-083: same plumbing as `crate::oauth::GoogleOauth`'s impl -- see
/// [`TokenRefresh`]'s doc comment.
impl<H: HttpForm> TokenRefresh for MicrosoftOauth<H> {
    fn refresh(&self, refresh_token: &str) -> Result<TokenSet, OauthError> {
        MicrosoftOauth::refresh(self, refresh_token)
    }
}

/// D18: Microsoft 365 / Outlook.com via IMAP XOAUTH2 (T-020). Wire protocol
/// is shared with Gmail in `xoauth2.rs`; only this marker type differs.
#[derive(Clone, Debug, Default)]
pub struct MicrosoftImap;

impl MailConnector for MicrosoftImap {
    fn probe(&self, form: &MailboxForm, access_token: &str) -> Result<ConnectOk, ConnectError> {
        probe_xoauth2(form, access_token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xoauth2::fake_server::{spawn_imap, spawn_smtp};
    use feathermail_core::{ApplyError, CoreError, ErrorCode};
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::thread;

    // ---- OAuth ----

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
            assert_eq!(url, MICROSOFT_TOKEN_URL);
            self.calls.lock().unwrap().push(
                form.iter()
                    .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                    .collect(),
            );
            Ok((self.status, self.body.clone()))
        }
    }

    fn cfg() -> MicrosoftClientConfig {
        MicrosoftClientConfig {
            client_id: "11111111-2222-3333-4444-555555555555".into(),
            client_secret: String::new(),
        }
    }

    #[test]
    fn authorization_url_has_pkce_and_outlook_scope() {
        let oauth = MicrosoftOauth::new(cfg(), MapHttp::new(200, "{}"));
        let pkce = Pkce::from_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".into());
        let url = oauth.authorization_url("http://127.0.0.1:8765", &pkce, "abc");
        assert!(url.starts_with(MICROSOFT_AUTH_URL));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("code_challenge=E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"));
        assert!(url.contains(&enc(MICROSOFT_SCOPE)));
        assert!(url.contains(&format!("client_id={}", enc(&oauth.config.client_id))));
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A8765"));
        assert!(url.contains("state=abc"));
    }

    #[test]
    fn exchange_sends_pkce_verifier_no_secret_by_default() {
        let http = MapHttp::new(
            200,
            r#"{"access_token":"at","refresh_token":"rt","expires_in":10}"#,
        );
        let oauth = MicrosoftOauth::new(cfg(), http);
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
        assert!(!calls[0].iter().any(|(k, _)| k == "client_secret"));
        assert!(!format!("{tokens:?}").contains("at"));
    }

    #[test]
    fn refresh_without_ui() {
        let http = MapHttp::new(200, r#"{"access_token":"new-token","expires_in":3600}"#);
        let oauth = MicrosoftOauth::new(cfg(), http);
        let tokens = oauth.refresh("rt-keep").unwrap();
        assert_eq!(tokens.access_token, "new-token");
        assert!(tokens.refresh_token.is_none());
        let calls = oauth.http.calls.lock().unwrap();
        assert!(calls[0]
            .iter()
            .any(|(k, v)| k == "refresh_token" && v == "rt-keep"));
        assert!(!format!("{tokens:?}").contains("new-token"));
    }

    #[test]
    fn revoked_refresh_is_auth_not_retry() {
        let http = MapHttp::new(400, r#"{"error":"invalid_grant"}"#);
        let oauth = MicrosoftOauth::new(cfg(), http);
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
    fn load_skips_placeholder_and_prefers_env() {
        let dir = tempfile_path();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("oauth.toml");
        std::fs::write(
            &path,
            "[microsoft]\nclient_id = \"YOUR_APPLICATION_ID\"\nclient_secret = \"YOUR_SECRET\"\n",
        )
        .unwrap();
        let err = MicrosoftClientConfig::load_from(None, None, &path, None, None).unwrap_err();
        assert!(matches!(err, OauthError::NotConfigured { .. }));
        if let OauthError::NotConfigured { message } = &err {
            assert!(message.contains("Microsoft"));
        }
        let cfg = MicrosoftClientConfig::load_from(
            Some("env-app-id".into()),
            None,
            &path,
            Some("build-id"),
            None,
        )
        .unwrap();
        assert_eq!(cfg.client_id, "env-app-id");
        assert_eq!(cfg.client_secret, "");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn loopback_captures_code() {
        let oauth = MicrosoftOauth::new(cfg(), MapHttp::new(200, "{}"));
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
    fn pkce_and_token_debug_redact_secrets() {
        let pkce = Pkce::generate().unwrap();
        assert!(!format!("{pkce:?}").contains(&pkce.verifier));

        let http = MapHttp::new(
            200,
            r#"{"access_token":"super-secret-at","refresh_token":"super-secret-rt","expires_in":5}"#,
        );
        let oauth = MicrosoftOauth::new(cfg(), http);
        let tokens = oauth
            .exchange("http://127.0.0.1:1", &Pkce::from_verifier("v".into()), "c")
            .unwrap();
        let debug = format!("{tokens:?}");
        assert!(!debug.contains("super-secret-at"));
        assert!(!debug.contains("super-secret-rt"));
    }

    fn tempfile_path() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "feathermail-msoauth-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        p
    }

    // ---- IMAP/SMTP XOAUTH2 ----

    fn form(imap_port: u16, smtp_port: u16) -> MailboxForm {
        crate::xoauth2::plaintext_form("you@outlook.com", imap_port, smtp_port)
    }

    #[test]
    fn xoauth2_ok_and_capabilities() {
        let imap = spawn_imap("eyJ0.good");
        let smtp = spawn_smtp();
        thread::sleep(Duration::from_millis(30));
        let ok = MicrosoftImap.probe(&form(imap, smtp), "eyJ0.good").unwrap();
        assert!(ok.capabilities.iter().any(|c| c == "AUTH=XOAUTH2"));
    }

    #[test]
    fn revoked_token_is_human() {
        let imap = spawn_imap("eyJ0.good");
        let smtp = spawn_smtp();
        thread::sleep(Duration::from_millis(30));
        let err = MicrosoftImap
            .probe(&form(imap, smtp), "eyJ0.revoked")
            .unwrap_err();
        match err {
            ConnectError::Auth { message, details } => {
                assert_eq!(message, ErrorCode::AuthRequired.default_message());
                assert!(!message.to_ascii_lowercase().contains("xoauth"));
                assert!(!message.to_ascii_lowercase().contains("imap"));
                let details = details.unwrap_or_default();
                assert!(!details.contains("eyJ0.revoked"));
            }
            other => panic!("{other:?}"),
        }
    }
}
