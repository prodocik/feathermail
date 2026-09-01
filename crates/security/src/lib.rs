//! Keyring (T-010) and log redaction (T-004).
//!
//! UI, MCP, and shortcuts talk to Core. This crate does not import GTK.

mod redact;
mod secret;

pub use redact::{init_tracing, redact_text, sensitive_field, REDACTED};
pub use secret::{
    LibsecretStore, MemorySecretStore, SecretError, SecretKey, SecretKind, SecretStore,
    SecretString, APP_ID,
};

/// Workspace probe so `cargo test -p feathermail-security` has a test.
pub fn crate_name() -> &'static str {
    env!("CARGO_PKG_NAME")
}

#[cfg(test)]
mod tests {
    use super::crate_name;

    #[test]
    fn crate_compiles() {
        assert!(crate_name().starts_with("feathermail-"));
    }
}
