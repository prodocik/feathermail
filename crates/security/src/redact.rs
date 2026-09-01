//! Strip passwords, tokens, message bodies, and attachment bytes from logs.

use std::fmt;

use tracing::field::{Field, Visit};
use tracing_subscriber::field::RecordFields;
use tracing_subscriber::fmt::format::{FormatFields, Writer};
use tracing_subscriber::EnvFilter;

pub const REDACTED: &str = "[redacted]";

const SENSITIVE_KEYS: &[&str] = &[
    "refresh_token",
    "access_token",
    "authorization",
    "password",
    "passwd",
    "secret",
    "token",
    "bearer",
];

pub fn sensitive_field(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.contains("password")
        || n.contains("passwd")
        || n.contains("secret")
        || n.contains("authorization")
        || n == "token"
        || n.ends_with("token")
        || n.contains("_token")
        || n == "body"
        || n == "body_html"
        || n == "body_text"
        || n == "raw_body"
        || n == "html"
        || n.ends_with("_body")
        || n == "bytes"
        || n == "attachment"
        || n.contains("attachment")
}

pub fn redact_text(input: &str) -> String {
    let lower = input.to_ascii_lowercase();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    let bytes = input.as_bytes();
    while i < bytes.len() {
        if let Some((key_len, consumed)) = match_secret_at(&lower, input, i) {
            out.push_str(&input[i..i + key_len]);
            out.push('=');
            out.push_str(REDACTED);
            i += consumed;
            continue;
        }
        let ch = input[i..].chars().next().expect("char");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn match_secret_at(lower: &str, original: &str, i: usize) -> Option<(usize, usize)> {
    for key in SENSITIVE_KEYS {
        if lower[i..].starts_with(key) {
            let after_key = i + key.len();
            let rest = original.get(after_key..)?;
            let trimmed = rest.trim_start_matches([' ', '\t', '"', '\'']);
            let skipped = rest.len() - trimmed.len();
            let sep = trimmed.chars().next()?;
            if sep != '=' && sep != ':' {
                continue;
            }
            let after_sep = &trimmed[sep.len_utf8()..];
            let skipped_ws = after_sep.len() - after_sep.trim_start().len();
            let value = after_sep.trim_start();
            let value_len = value
                .chars()
                .take_while(|c| !c.is_whitespace() && *c != '"' && *c != '\'')
                .map(char::len_utf8)
                .sum::<usize>();
            if value_len == 0 {
                continue;
            }
            let consumed = key.len() + skipped + sep.len_utf8() + skipped_ws + value_len;
            return Some((key.len(), consumed));
        }
    }
    None
}

struct FieldVisitor<'a> {
    writer: Writer<'a>,
    first: bool,
}

impl Visit for FieldVisitor<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let raw = if sensitive_field(field.name()) {
            REDACTED.to_string()
        } else {
            redact_text(&format!("{value:?}"))
        };
        let _ = self.write_pair(field.name(), raw);
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        let raw = if sensitive_field(field.name()) {
            REDACTED.to_string()
        } else {
            redact_text(value)
        };
        let _ = self.write_pair(field.name(), raw);
    }
}

impl FieldVisitor<'_> {
    fn write_pair(&mut self, name: &str, value: String) -> fmt::Result {
        if !self.first {
            write!(self.writer, " ")?;
        }
        self.first = false;
        if name == "message" {
            write!(self.writer, "{value}")
        } else {
            write!(self.writer, "{name}={value}")
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct RedactingFields;

impl<'writer> FormatFields<'writer> for RedactingFields {
    fn format_fields<R: RecordFields>(&self, writer: Writer<'writer>, fields: R) -> fmt::Result {
        let mut visitor = FieldVisitor {
            writer,
            first: true,
        };
        fields.record(&mut visitor);
        Ok(())
    }
}

/// Default filter: INFO. `RUST_LOG` can raise to debug in development.
pub fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .fmt_fields(RedactingFields)
        .with_target(true)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::fmt::MakeWriter;

    #[derive(Clone)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl<'a> MakeWriter<'a> for Capture {
        type Writer = GuardWriter;

        fn make_writer(&'a self) -> Self::Writer {
            GuardWriter(self.0.clone())
        }
    }

    struct GuardWriter(Arc<Mutex<Vec<u8>>>);

    impl io::Write for GuardWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("log buffer").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn field_names() {
        assert!(sensitive_field("password"));
        assert!(sensitive_field("refresh_token"));
        assert!(sensitive_field("body_html"));
        assert!(sensitive_field("attachment_bytes"));
        assert!(!sensitive_field("subject"));
        assert!(!sensitive_field("account_id"));
    }

    #[test]
    fn text_redacts_password_assignment() {
        let out = redact_text("password=hunter2 extra");
        assert!(!out.contains("hunter2"), "{out}");
        assert!(out.contains(REDACTED), "{out}");
    }

    #[test]
    fn log_with_password_does_not_contain_secret() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .fmt_fields(RedactingFields)
            .with_writer(Capture(buf.clone()))
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(password = "hunter2", "login attempt");
            tracing::info!(body = "<p>private letter</p>", "stored message");
            tracing::info!(token = "ya29.secret", "oauth");
        });
        let bytes = buf.lock().expect("log buffer").clone();
        let out = String::from_utf8_lossy(&bytes);
        assert!(!out.contains("hunter2"), "{out}");
        assert!(!out.contains("private letter"), "{out}");
        assert!(!out.contains("ya29.secret"), "{out}");
        assert!(out.contains(REDACTED), "{out}");
    }
}
