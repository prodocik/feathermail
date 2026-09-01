//! T-156: the door between the "Add account" wizard's Other-IMAP page and
//! `feathermail_providers::autoconfig_lookup` (D20: Mozilla ISPDB over
//! HTTPS, then SRV `_imaps`/`_submission` per RFC 6186).
//!
//! Why a thread and not a worker command: the lookup is a real network
//! round-trip (up to 5 s of HTTP plus two 3 s DNS waits) and the shell
//! calls it from a GTK entry callback, where D11 allows 16 ms. So the same
//! shape [`crate::provision::spawn_provision`] already uses — one attempt,
//! one short-lived thread, exactly one `sink` call — with one addition: a
//! watchdog thread that caps the whole attempt at [`LOOKUP_TIMEOUT`], so a
//! nameserver that never answers cannot leave the wizard's spinner up
//! longer than the user is willing to wait. The inner thread is allowed to
//! finish into a dropped channel afterwards; its result is discarded.
//!
//! D14: the address is never logged here (this module logs nothing at all)
//! and never reaches `Debug` — [`AutoconfigOutcome`]'s `Debug` is
//! hand-written and redacts it, the way `ProvisionRequest`'s redacts the
//! password. Provider error text is human-only and is dropped whenever it
//! would echo the address back.
//!
//! D9: the shell calls this; it does not call `feathermail-providers`.

use std::sync::mpsc;
use std::time::Duration;

use feathermail_core::MailboxForm;
use feathermail_providers::{
    autoconfig_lookup, AutoconfigError, DnsSrv, HttpGet, LiveDns, LiveHttp,
};

/// Whole-attempt cap. The provider halves already time out on their own
/// (5 s HTTP + 3 s per DNS query), but they run in sequence, so their sum
/// is larger than any wizard should hold a spinner for.
pub const LOOKUP_TIMEOUT: Duration = Duration::from_secs(8);

/// What one autoconfig attempt found. `NotFound` and `Failed` are the same
/// thing for the wizard — fall back to manual entry — but only `Failed`
/// has something worth showing the user.
#[derive(Clone, PartialEq, Eq)]
pub enum AutoconfigOutcome {
    /// ISPDB and/or SRV answered with a usable IMAP + SMTP pair.
    Found(MailboxForm),
    /// The provider is not in the ISPDB and publishes no SRV records.
    NotFound,
    /// The lookup could not be completed (network, DNS, timeout, or an
    /// address with no domain). Human text, safe to show in the wizard.
    Failed(String),
}

/// D14: the form carries the user's address; `Debug` must not.
impl std::fmt::Debug for AutoconfigOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Found(form) => f
                .debug_struct("Found")
                .field("email", &"[redacted]")
                .field("imap_host", &form.imap_host)
                .field("imap_port", &form.imap_port)
                .field("imap_security", &form.imap_security)
                .field("smtp_host", &form.smtp_host)
                .field("smtp_port", &form.smtp_port)
                .field("smtp_security", &form.smtp_security)
                .finish(),
            Self::NotFound => f.write_str("NotFound"),
            Self::Failed(message) => f.debug_tuple("Failed").field(message).finish(),
        }
    }
}

/// Spawn the one-shot autoconfig thread. Returns immediately (D11), and
/// calls `sink` exactly once, from a spawned thread, within
/// [`LOOKUP_TIMEOUT`].
pub fn spawn_autoconfig(email: String, sink: impl FnOnce(AutoconfigOutcome) + Send + 'static) {
    spawn_with(email, LiveHttp, LiveDns, LOOKUP_TIMEOUT, sink);
}

/// [`spawn_autoconfig`] with the network halves and the cap injected, so
/// the thread/watchdog/`sink`-once contract is testable offline.
fn spawn_with<H, D>(
    email: String,
    http: H,
    dns: D,
    timeout: Duration,
    sink: impl FnOnce(AutoconfigOutcome) + Send + 'static,
) where
    H: HttpGet + Send + 'static,
    D: DnsSrv + Send + 'static,
{
    std::thread::spawn(move || {
        let (tx, rx) = mpsc::channel();
        // The lookup runs one level deeper so this thread can give up on
        // it: `send` into a dropped channel is an ignored error, and a
        // panic in the lookup drops the sender, which arrives here as
        // `Disconnected` -- either way `sink` still runs exactly once.
        std::thread::spawn(move || {
            let _ = tx.send(lookup(&email, &http, &dns));
        });
        let outcome = match rx.recv_timeout(timeout) {
            Ok(outcome) => outcome,
            Err(mpsc::RecvTimeoutError::Timeout) => AutoconfigOutcome::Failed(
                "Looking up this provider's settings took too long. \
                 Enter the server details below."
                    .to_string(),
            ),
            Err(mpsc::RecvTimeoutError::Disconnected) => AutoconfigOutcome::Failed(
                "Looking up this provider's settings failed. \
                 Enter the server details below."
                    .to_string(),
            ),
        };
        sink(outcome);
    });
}

fn lookup(email: &str, http: &impl HttpGet, dns: &impl DnsSrv) -> AutoconfigOutcome {
    match autoconfig_lookup(email, http, dns) {
        Ok(Some(form)) => AutoconfigOutcome::Found(form),
        Ok(None) => AutoconfigOutcome::NotFound,
        Err(err) => AutoconfigOutcome::Failed(failure_message(&err, email)),
    }
}

/// D14: provider errors are built from URLs and socket text, which can
/// carry the domain the user typed; anything that echoes the address back
/// is replaced with the generic sentence rather than trimmed.
fn failure_message(err: &AutoconfigError, email: &str) -> String {
    let detail = err.message.trim();
    let leaks = {
        let lowered = detail.to_ascii_lowercase();
        let email = email.trim().to_ascii_lowercase();
        !email.is_empty() && lowered.contains(&email)
    };
    if detail.is_empty() || leaks {
        "Looking up this provider's settings failed. Enter the server details below.".to_string()
    } else {
        detail.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use feathermail_core::MailSecurity;
    use feathermail_providers::SrvRecord;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    const XML: &str = r#"
<clientConfig>
  <emailProvider>
    <incomingServer type="imap">
      <hostname>imap.example.com</hostname>
      <port>993</port>
      <socketType>SSL</socketType>
    </incomingServer>
    <outgoingServer type="smtp">
      <hostname>smtp.example.com</hostname>
      <port>587</port>
      <socketType>STARTTLS</socketType>
    </outgoingServer>
  </emailProvider>
</clientConfig>
"#;

    /// An ISPDB double that also records every URL it was asked for, so a
    /// test can assert the transport (D20 says ISPDB, over TLS).
    #[derive(Clone, Default)]
    struct MapHttp {
        pages: HashMap<String, String>,
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl MapHttp {
        fn with(url: &str, body: &str) -> Self {
            let mut pages = HashMap::new();
            pages.insert(url.to_string(), body.to_string());
            Self {
                pages,
                seen: Arc::default(),
            }
        }
    }

    impl HttpGet for MapHttp {
        fn get(&self, url: &str) -> Result<Option<String>, AutoconfigError> {
            self.seen.lock().unwrap().push(url.to_string());
            // `None` is exactly what `LiveHttp` returns for a 404.
            Ok(self.pages.get(url).cloned())
        }
    }

    struct DeadHttp;

    impl HttpGet for DeadHttp {
        fn get(&self, _url: &str) -> Result<Option<String>, AutoconfigError> {
            Err(AutoconfigError {
                message: "the network is down".into(),
            })
        }
    }

    /// An ISPDB double that never answers in time.
    struct SlowHttp(Duration);

    impl HttpGet for SlowHttp {
        fn get(&self, _url: &str) -> Result<Option<String>, AutoconfigError> {
            std::thread::sleep(self.0);
            Ok(None)
        }
    }

    #[derive(Clone, Default)]
    struct MapDns(HashMap<String, Vec<SrvRecord>>);

    impl DnsSrv for MapDns {
        fn srv(&self, name: &str) -> Result<Vec<SrvRecord>, AutoconfigError> {
            Ok(self.0.get(name).cloned().unwrap_or_default())
        }
    }

    fn ispdb_http() -> MapHttp {
        MapHttp::with("https://autoconfig.thunderbird.net/v1.1/example.com", XML)
    }

    /// Runs `spawn_with` and returns (outcome, how many times the sink ran).
    fn run(
        email: &str,
        http: impl HttpGet + Send + 'static,
        dns: impl DnsSrv + Send + 'static,
        timeout: Duration,
    ) -> (AutoconfigOutcome, usize) {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let (tx, rx) = mpsc::channel();
        spawn_with(email.to_string(), http, dns, timeout, move |outcome| {
            counter.fetch_add(1, Ordering::SeqCst);
            let _ = tx.send(outcome);
        });
        let outcome = rx
            .recv_timeout(Duration::from_secs(30))
            .expect("the sink must be called");
        // A second call would arrive here; the wizard replaces the whole
        // form on `Found`, so twice is a visible bug, not a harmless one.
        assert!(
            rx.recv_timeout(Duration::from_millis(250)).is_err(),
            "the sink must be called exactly once"
        );
        (outcome, calls.load(Ordering::SeqCst))
    }

    #[test]
    fn ispdb_answer_prefills_the_form() {
        let (outcome, calls) = run(
            "you@example.com",
            ispdb_http(),
            MapDns::default(),
            LOOKUP_TIMEOUT,
        );
        let AutoconfigOutcome::Found(form) = outcome else {
            panic!("an ISPDB hit must prefill the form, got {outcome:?}");
        };
        assert_eq!(form.email, "you@example.com");
        assert_eq!(form.imap_host, "imap.example.com");
        assert_eq!(form.imap_port, 993);
        assert_eq!(form.imap_security, MailSecurity::Ssl);
        assert_eq!(form.smtp_host, "smtp.example.com");
        assert_eq!(form.smtp_port, 587);
        assert_eq!(form.smtp_security, MailSecurity::StartTls);
        assert_eq!(calls, 1);
    }

    /// D20 names the ISPDB as an HTTPS endpoint: the wizard hands an
    /// address to a third party, so the request must not be interceptable.
    #[test]
    fn the_ispdb_is_only_ever_asked_over_tls() {
        let http = ispdb_http();
        let seen = Arc::clone(&http.seen);
        let (outcome, _) = run("you@example.com", http, MapDns::default(), LOOKUP_TIMEOUT);
        assert!(matches!(outcome, AutoconfigOutcome::Found(_)));
        let urls = seen.lock().unwrap().clone();
        assert!(!urls.is_empty(), "the ISPDB must have been asked");
        for url in urls {
            assert!(url.starts_with("https://"), "plaintext autoconfig: {url}");
        }
    }

    #[test]
    fn a_404_and_no_srv_records_is_not_found() {
        let (outcome, calls) = run(
            "you@no-such.example",
            MapHttp::default(),
            MapDns::default(),
            LOOKUP_TIMEOUT,
        );
        assert_eq!(outcome, AutoconfigOutcome::NotFound, "{outcome:?}");
        assert_eq!(calls, 1);
    }

    #[test]
    fn srv_records_alone_prefill_the_form() {
        let mut dns = HashMap::new();
        dns.insert(
            "_imaps._tcp.example.com".to_string(),
            vec![SrvRecord {
                priority: 0,
                port: 993,
                target: "mail.example.com.".into(),
            }],
        );
        dns.insert(
            "_submission._tcp.example.com".to_string(),
            vec![SrvRecord {
                priority: 10,
                port: 587,
                target: "smtp.example.com.".into(),
            }],
        );
        let (outcome, _) = run(
            "you@example.com",
            MapHttp::default(),
            MapDns(dns),
            LOOKUP_TIMEOUT,
        );
        let AutoconfigOutcome::Found(form) = outcome else {
            panic!("SRV records must prefill the form, got {outcome:?}");
        };
        assert_eq!(form.imap_host, "mail.example.com");
        assert_eq!(form.smtp_host, "smtp.example.com");
    }

    #[test]
    fn a_network_error_is_reported_not_swallowed() {
        let (outcome, calls) = run(
            "you@example.com",
            DeadHttp,
            MapDns::default(),
            LOOKUP_TIMEOUT,
        );
        let AutoconfigOutcome::Failed(message) = outcome else {
            panic!("a dead network must fail, not answer NotFound: {outcome:?}");
        };
        assert!(message.contains("the network is down"), "{message}");
        assert_eq!(calls, 1);
    }

    /// The whole attempt is capped: a nameserver or ISPDB that never
    /// answers must still release the wizard, once.
    #[test]
    fn a_hung_lookup_gives_up_at_the_cap() {
        let started = std::time::Instant::now();
        let (outcome, calls) = run(
            "you@example.com",
            SlowHttp(Duration::from_secs(20)),
            MapDns::default(),
            Duration::from_millis(50),
        );
        let AutoconfigOutcome::Failed(message) = outcome else {
            panic!("a hung lookup must time out, got {outcome:?}");
        };
        assert!(message.contains("took too long"), "{message}");
        assert_eq!(calls, 1);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the cap must not wait for the hung half"
        );
        assert!(
            LOOKUP_TIMEOUT <= Duration::from_secs(8),
            "the production cap must stay inside what a wizard may hold"
        );
    }

    /// Every wizard keystroke is a possible address; none of them may
    /// panic, and none may hang the sink.
    #[test]
    fn any_address_shape_answers_without_panicking() {
        for email in [
            "",
            "@",
            "you@",
            "@example.com",
            "you",
            "  ",
            "юзер@пример.рф",
        ] {
            let (outcome, calls) =
                run(email, MapHttp::default(), MapDns::default(), LOOKUP_TIMEOUT);
            assert_eq!(calls, 1, "{email:?} called the sink {calls} times");
            assert!(
                matches!(
                    outcome,
                    AutoconfigOutcome::NotFound | AutoconfigOutcome::Failed(_)
                ),
                "{email:?} must not produce a form: {outcome:?}"
            );
        }
    }

    /// D14: the outcome is the value the shell will hand to `tracing` or a
    /// panic message the day someone adds one; the address must not be in
    /// it -- neither in `Debug` nor inside a provider error string.
    #[test]
    fn neither_debug_nor_an_error_carries_the_address() {
        let (found, _) = run(
            "you@example.com",
            ispdb_http(),
            MapDns::default(),
            LOOKUP_TIMEOUT,
        );
        let text = format!("{found:?}");
        assert!(!text.contains("you@example.com"), "{text}");
        assert!(text.contains("[redacted]"), "{text}");
        assert!(text.contains("imap.example.com"), "{text}");

        let echoed = AutoconfigError {
            message: "no route to host for you@example.com".into(),
        };
        let message = failure_message(&echoed, "You@Example.com");
        assert!(!message.contains("example.com"), "{message}");
        assert!(message.contains("Enter the server details"), "{message}");
    }
}
