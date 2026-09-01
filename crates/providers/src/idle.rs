//! IMAP IDLE and the honest poll fallback (T-026, ТЗ §22, §63, D30; RFC
//! 2177).
//!
//! `feathermail-sync::connection` (this workspace, `crates/sync`) decides
//! *whether* we should be connected at all (online/offline/reconnect
//! backoff) using no clock and no sockets of its own. This module is the
//! other half, on the side that actually owns the socket (D9): once a
//! session is up, *keep it usefully idle* — wait for the server to say
//! something, without either busy-looping or holding the connection open
//! past the point servers are allowed to drop it.
//!
//! ## What one round does
//!
//! - If the server never advertised `IDLE` in `CAPABILITY`, there is
//!   nothing to wait on the wire for — falling silent here would look like
//!   "everything's fine" while actually just never checking again. D30 is
//!   explicit that the fallback is a poll, so [`run_idle_with`] issues a
//!   `NOOP` (any mailbox change can ride along on it as an untagged
//!   response) and reports what it found, honestly, every time it's called.
//! - If the server does support IDLE, send `IDLE`, then wait for either an
//!   untagged `EXISTS`/`EXPUNGE`/`FETCH` (something happened — return
//!   promptly so sync, T-022, finds out now rather than up to 29 minutes
//!   from now), the caller's stop signal, or the ~29-minute ceiling —
//!   whichever comes first — then always send `DONE` before returning, so
//!   the session is left in ordinary command/response mode no matter which
//!   of the three ended the round.
//!
//! ## Why the timeout matters (RFC 2177)
//!
//! RFC 2177 §3: "the server MAY consider a client inactive if it has an
//! IDLE command running, and if such a server has an inactivity timeout it
//! MAY log the client off implicitly at the end of its timeout period ...
//! Because of that, the client SHOULD terminate the IDLE command and
//! re-issue it at least every 29 minutes." Waiting for the server to hang
//! up on us and only *then* reconnecting would be indistinguishable, from
//! the outside, from a flaky network — and would hand the ban on
//! reconnect storms nothing to grab onto, since it would look like an
//! ordinary drop-and-retry every time, forever. Re-issuing IDLE ourselves,
//! comfortably inside the server's 30-minute allowance, avoids ever
//! reaching that edge at all.

use std::time::{Duration, Instant};

use feathermail_core::ConnectError;

use crate::session::{Capabilities, IdleEvent, ImapSession};

/// RFC 2177's 30-minute server-side allowance, minus a one-minute margin
/// for round-trip time and scheduling jitter. [`run_idle`] uses this as the
/// default; tests exercise the timeout path with a much shorter duration
/// via [`run_idle_with`] instead of waiting 29 real minutes.
pub const IDLE_TIMEOUT_SECS: u64 = 29 * 60;

/// How often an active IDLE gets to check the overall ceiling / stop signal
/// while nothing has happened on the wire. Small relative to
/// [`IDLE_TIMEOUT_SECS`] so the ~29 minute cutoff is honored with
/// reasonably tight precision without busy-looping on the socket.
const DEFAULT_POLL_SLICE: Duration = Duration::from_secs(5);

/// Recommended cadence for calling [`run_idle`] again in the no-IDLE
/// fallback path (D30: "poll 60s foreground"). This module does not sleep
/// on its own behalf — it has no driver loop or thread of its own — so a
/// caller wiring this into an actual event loop is expected to wait about
/// this long between calls when [`Capabilities::idle`] is false, exactly
/// the same "policy lives with the caller, not the socket code" split
/// `feathermail_sync::schedule` uses for its own intervals.
pub const NO_IDLE_POLL_SECS: u64 = 60;

/// What one round of [`run_idle`]/[`run_idle_with`] found.
#[derive(Debug, PartialEq, Eq)]
pub enum IdleOutcome {
    /// The server said something (IDLE path), or a `NOOP` turned up
    /// mailbox-change responses (no-IDLE path).
    Events(Vec<IdleEvent>),
    /// Nothing happened before the ceiling — for the IDLE path this means
    /// the ~29 minute timeout fired (DONE was already sent; re-issue IDLE
    /// to keep watching); for the no-IDLE path it means this one poll saw
    /// no changes (call again after [`NO_IDLE_POLL_SECS`]).
    TimedOut,
    /// The caller's stop signal fired. For the IDLE path, `DONE` was sent
    /// before returning; for the no-IDLE path nothing was outstanding to
    /// clean up.
    Stopped,
}

/// Run one IDLE round (or, without IDLE, one honest poll) using the real
/// RFC 2177 timeout and poll cadence. See the module doc comment for the
/// full behavior; see [`run_idle_with`] for the version tests use to avoid
/// a 29-minute wait.
pub fn run_idle(
    session: &mut ImapSession,
    caps: &Capabilities,
    should_stop: impl FnMut() -> bool,
) -> Result<IdleOutcome, ConnectError> {
    run_idle_with(
        session,
        caps,
        Duration::from_secs(IDLE_TIMEOUT_SECS),
        DEFAULT_POLL_SLICE,
        should_stop,
    )
}

/// [`run_idle`] with the ceiling and poll slice as explicit parameters, so
/// the timeout behavior is testable in milliseconds instead of minutes.
/// `should_stop` is checked before blocking and again on every poll slice,
/// mirroring the cancellation-check pattern `feathermail_sync::sync_folder`
/// already uses between batches (D11) — an external "stop now" signal
/// (e.g. connection state moved to `Offline`/`Reconnecting`) is honored
/// promptly rather than only between whole IDLE rounds.
pub fn run_idle_with(
    session: &mut ImapSession,
    caps: &Capabilities,
    idle_timeout: Duration,
    poll_slice: Duration,
    mut should_stop: impl FnMut() -> bool,
) -> Result<IdleOutcome, ConnectError> {
    if !caps.idle {
        if should_stop() {
            return Ok(IdleOutcome::Stopped);
        }
        let events = session.noop_check()?;
        return Ok(if events.is_empty() {
            IdleOutcome::TimedOut
        } else {
            IdleOutcome::Events(events)
        });
    }

    session.idle_start()?;
    let start = Instant::now();
    loop {
        if should_stop() {
            session.idle_done()?;
            return Ok(IdleOutcome::Stopped);
        }
        let elapsed = start.elapsed();
        if elapsed >= idle_timeout {
            session.idle_done()?;
            return Ok(IdleOutcome::TimedOut);
        }
        let remaining = idle_timeout - elapsed;
        let slice = poll_slice.min(remaining);
        match session.idle_poll(slice)? {
            Some(event) => {
                // Return on the first sign of change rather than batching
                // for the rest of the window: sync (T-022) wants to know
                // now, not up to 29 minutes from now. A caller that wants
                // more can just call `run_idle` again immediately.
                session.idle_done()?;
                return Ok(IdleOutcome::Events(vec![event]));
            }
            None => continue, // one poll slice elapsed with nothing on the wire
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{ImapAuth, SelectedMailbox};
    use feathermail_core::{MailSecurity, MailboxForm};
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::thread;

    fn form(port: u16) -> MailboxForm {
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

    fn idle_capable() -> Capabilities {
        Capabilities {
            idle: true,
            ..Capabilities::default()
        }
    }

    fn no_idle() -> Capabilities {
        Capabilities::default()
    }

    /// Fake server: LOGIN, then whatever the test closure over the reader
    /// half wants to do (IDLE/DONE/NOOP handling is scripted per test).
    fn spawn(
        script: impl FnOnce(BufReader<std::net::TcpStream>, std::net::TcpStream) + Send + 'static,
    ) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            write!(writer, "* OK fake ready\r\n").unwrap();
            writer.flush().unwrap();
            let mut line = String::new();
            let _ = reader.read_line(&mut line); // LOGIN
            let tag = line.split_whitespace().next().unwrap_or("*").to_string();
            write!(writer, "{tag} OK logged in\r\n").unwrap();
            writer.flush().unwrap();
            script(reader, writer);
        });
        port
    }

    fn connect(port: u16) -> ImapSession {
        thread::sleep(std::time::Duration::from_millis(30));
        ImapSession::connect(&form(port), ImapAuth::Login("x".into())).unwrap()
    }

    /// The server pushes an unsolicited EXISTS while we're idling: the
    /// round must return that event, having sent DONE (proven by the fake
    /// server successfully reading the DONE line and replying OK — if
    /// `run_idle_with` didn't send DONE, the fake server's `read_line`
    /// for it would block and the test would hang/panic on drop).
    #[test]
    fn idle_reports_exists_and_sends_done() {
        let port = spawn(|mut reader, mut writer| {
            let mut line = String::new();
            let _ = reader.read_line(&mut line); // "Sx IDLE"
            let tag = line.split_whitespace().next().unwrap_or("S1").to_string();
            write!(writer, "+ idling\r\n").unwrap();
            writer.flush().unwrap();
            write!(writer, "* 7 EXISTS\r\n").unwrap();
            writer.flush().unwrap();
            let mut done = String::new();
            let _ = reader.read_line(&mut done); // "DONE"
            assert_eq!(done.trim(), "DONE");
            write!(writer, "{tag} OK IDLE terminated\r\n").unwrap();
            writer.flush().unwrap();
        });
        let mut session = connect(port);
        let outcome = run_idle_with(
            &mut session,
            &idle_capable(),
            Duration::from_secs(5),
            Duration::from_millis(20),
            || false,
        )
        .unwrap();
        assert_eq!(
            outcome,
            IdleOutcome::Events(vec![IdleEvent::Exists(7)]),
            "must surface the untagged EXISTS as an event"
        );
    }

    /// Nothing happens on the wire at all: the round must give up and send
    /// DONE once `idle_timeout` elapses — well before any real 29/30 minute
    /// wait — rather than blocking forever. This is the core RFC 2177
    /// requirement, exercised with a tiny timeout instead of a real one.
    #[test]
    fn idle_times_out_before_the_ceiling_and_sends_done() {
        let port = spawn(|mut reader, mut writer| {
            let mut line = String::new();
            let _ = reader.read_line(&mut line); // "Sx IDLE"
            let tag = line.split_whitespace().next().unwrap_or("S1").to_string();
            write!(writer, "+ idling\r\n").unwrap();
            writer.flush().unwrap();
            // Deliberately silent: the client must give up on its own.
            let mut done = String::new();
            let _ = reader.read_line(&mut done);
            assert_eq!(
                done.trim(),
                "DONE",
                "client must send DONE once its timeout elapses"
            );
            write!(writer, "{tag} OK IDLE terminated\r\n").unwrap();
            writer.flush().unwrap();
        });
        let mut session = connect(port);
        let start = Instant::now();
        let outcome = run_idle_with(
            &mut session,
            &idle_capable(),
            Duration::from_millis(150),
            Duration::from_millis(20),
            || false,
        )
        .unwrap();
        assert_eq!(outcome, IdleOutcome::TimedOut);
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "must not block anywhere close to a real IDLE ceiling"
        );
    }

    /// The caller's stop signal must end the round promptly, with DONE
    /// still sent so the session comes back in a clean, non-IDLE state.
    #[test]
    fn stop_signal_ends_idle_and_still_sends_done() {
        let port = spawn(|mut reader, mut writer| {
            let mut line = String::new();
            let _ = reader.read_line(&mut line); // "Sx IDLE"
            let tag = line.split_whitespace().next().unwrap_or("S1").to_string();
            write!(writer, "+ idling\r\n").unwrap();
            writer.flush().unwrap();
            let mut done = String::new();
            let _ = reader.read_line(&mut done);
            assert_eq!(done.trim(), "DONE");
            write!(writer, "{tag} OK IDLE terminated\r\n").unwrap();
            writer.flush().unwrap();
        });
        let mut session = connect(port);
        let mut calls = 0u32;
        let outcome = run_idle_with(
            &mut session,
            &idle_capable(),
            Duration::from_secs(60),
            Duration::from_millis(20),
            || {
                calls += 1;
                true // stop immediately
            },
        )
        .unwrap();
        assert_eq!(outcome, IdleOutcome::Stopped);
        assert!(calls >= 1);
    }

    /// Server without IDLE in CAPABILITY: the fallback must issue a real
    /// NOOP (never silently do nothing) and report what it found.
    #[test]
    fn no_idle_capability_falls_back_to_noop_poll() {
        let port = spawn(|mut reader, mut writer| {
            let mut line = String::new();
            let _ = reader.read_line(&mut line); // "Sx NOOP"
            let tag = line.split_whitespace().next().unwrap_or("S1").to_string();
            let upper = line.to_ascii_uppercase();
            assert!(
                upper.contains("NOOP"),
                "no-IDLE fallback must actually poll, not do nothing"
            );
            write!(writer, "* 3 EXISTS\r\n").unwrap();
            write!(writer, "{tag} OK NOOP completed\r\n").unwrap();
            writer.flush().unwrap();
        });
        let mut session = connect(port);
        let outcome = run_idle_with(
            &mut session,
            &no_idle(),
            Duration::from_secs(5),
            Duration::from_millis(20),
            || false,
        )
        .unwrap();
        assert_eq!(outcome, IdleOutcome::Events(vec![IdleEvent::Exists(3)]));
    }

    /// No-IDLE fallback with nothing to report: an honest `TimedOut`, not a
    /// silent no-op, and the NOOP round trip still actually happened.
    #[test]
    fn no_idle_capability_reports_timed_out_when_nothing_changed() {
        let port = spawn(|mut reader, mut writer| {
            let mut line = String::new();
            let _ = reader.read_line(&mut line); // "Sx NOOP"
            let tag = line.split_whitespace().next().unwrap_or("S1").to_string();
            write!(writer, "{tag} OK NOOP completed\r\n").unwrap();
            writer.flush().unwrap();
        });
        let mut session = connect(port);
        let outcome = run_idle_with(
            &mut session,
            &no_idle(),
            Duration::from_secs(5),
            Duration::from_millis(20),
            || false,
        )
        .unwrap();
        assert_eq!(outcome, IdleOutcome::TimedOut);
    }

    /// No-IDLE fallback honors the stop signal too, without ever touching
    /// the socket (no NOOP is sent — nothing for the fake server to read,
    /// so a hang here would mean the stop check was skipped).
    #[test]
    fn no_idle_capability_honors_stop_signal_without_polling() {
        let port = spawn(|_reader, _writer| {
            // Deliberately does nothing: if the client sends NOOP anyway,
            // it never gets a reply, and `run_idle_with` would return an
            // error (or hang) instead of `Stopped`.
        });
        let mut session = connect(port);
        let outcome = run_idle_with(
            &mut session,
            &no_idle(),
            Duration::from_secs(5),
            Duration::from_millis(20),
            || true,
        )
        .unwrap();
        assert_eq!(outcome, IdleOutcome::Stopped);
    }

    /// Silences an unused-import warning if `SelectedMailbox` stops being
    /// needed by a future edit here; kept imported for parity with
    /// `session.rs`'s own test module style (explicit, narrow imports).
    #[allow(dead_code)]
    fn _unused(_: SelectedMailbox) {}
}
