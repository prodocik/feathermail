//! Connection state machine: online/offline, reconnect, resume (T-026, ТЗ
//! §22, §63, D30).
//!
//! Pure logic, like the rest of this crate: no sockets, no timers, no clock
//! of its own (`Instant::now`/`SystemTime::now` are never called here) —
//! `now: i64` arrives as a plain argument, exactly like [`crate::schedule`].
//! The actual socket work (dialing, reading IDLE responses, detecting a
//! dropped connection) lives in `feathermail-providers`; this module only
//! decides, given an event and a clock reading, what state the connection
//! is in and what should happen next. That split is what makes the
//! no-reconnect-storm property checkable without a real network, a real
//! clock, or a real IMAP server (see the tests below).
//!
//! ## The three states
//!
//! - [`ConnectionState::Online`] — connected, IDLE (or poll) and the
//!   operation queue should be running.
//! - [`ConnectionState::Offline`] — the network itself is unreachable.
//!   There is deliberately no timer here: nothing is dialed again until an
//!   external [`ConnectionEvent::NetworkRestored`] signal arrives (e.g. from
//!   NetworkManager, or a successful low-level probe). Retrying against a
//!   server while there is no route to it would just be the reconnect storm
//!   this ticket forbids, dressed up as "trying harder".
//! - [`ConnectionState::Reconnecting`] — the network is believed to be up,
//!   but the last connect attempt (or the last live connection) failed for
//!   some other reason: auth error, timeout, dropped mid-IDLE, a 5xx from
//!   the server. This axis *does* have a clock: [`due_now`] gates the next
//!   attempt behind the same D32 backoff table used everywhere else in this
//!   codebase (`next_attempt_at`, growing with `consecutive_failures`).
//!
//! ## Why "network gone" and "server error" have to be different states
//!
//! ТЗ §63 and this ticket both call out "no infinite reconnect storm" as a
//! single requirement, but it is actually two different failure modes that
//! both have to be handled without one:
//! - If the network is down, backing off and retrying on a timer is still
//!   wrong even at a slow cadence — there is nothing to reach, so every
//!   attempt is guaranteed to fail and all it does is spin a radio/CPU for
//!   no benefit until the network comes back on its own. The right amount
//!   of polling here is *zero*: wait for an external signal.
//! - If the network is fine but the server keeps rejecting us, silently
//!   waiting for an external signal that will never come is wrong too —
//!   this is exactly the case backoff exists for, and it must actually grow
//!   (2s, 5s, 15s, 30s, 60s, doubling, capped at 15 minutes; see
//!   [`backoff_floor_secs`]) or repeated failures degenerate into the same
//!   storm ТЗ §63 forbids, just relabeled.
//!
//! Conflating the two into one "disconnected" state with one clock is
//! exactly the bug this module is designed to not have: it would either
//! poll a dead network on a timer, or sit forever ignoring a live network
//! whose server is simply unhappy right now.
//!
//! ## `backoff_floor_secs`
//!
//! Deliberately the same D32 step table as `feathermail_core::retry_delay_secs`
//! (`crates/core/src/queue.rs`) and [`crate::schedule::backoff_floor_secs`]
//! (`crates/sync/src/schedule.rs`) — 2s, 5s, 15s, 30s, 60s, then doubling,
//! capped at 15 minutes. This crate has no dependency on `core` (so it can't
//! call the former), and `schedule`'s copy is private to that module (so it
//! can't be called from here either without changing that file's visibility,
//! which is out of scope for this ticket — see the task notes). Rather than
//! inventing a *third*, differently-shaped backoff curve, this is a
//! byte-for-byte copy of the same table, for the same reason `schedule.rs`
//! gives for its own copy: three independent backoff policies that quietly
//! drift apart is worse than one small, well-justified duplication.

/// Where the connection currently stands. See the module doc comment for
/// the full rationale behind each variant and why they're not collapsed
/// into one "disconnected" state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    /// Connected. IDLE (or the poll fallback) and the operation queue
    /// should both be running.
    Online,
    /// The network itself is unreachable. No internal clock advances this
    /// state; only [`ConnectionEvent::NetworkRestored`] does.
    Offline,
    /// Network believed reachable, but the last attempt (or the last live
    /// connection) failed for a non-network reason. `next_attempt_at` is
    /// the earliest `now` at which another attempt is due; it grows with
    /// `consecutive_failures` per [`backoff_floor_secs`].
    Reconnecting {
        consecutive_failures: u32,
        next_attempt_at: i64,
    },
}

/// Something that happened, driving a state transition. All classification
/// of *why* an attempt failed (was it "no route to host" vs. "server said
/// NO") happens on the caller's side, where the real socket/error lives
/// (D9: this crate has no sockets of its own) — see
/// [`ConnectionEvent::NetworkLost`] and [`ConnectionEvent::ConnectFailed`]
/// for the dividing line callers are expected to use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionEvent {
    /// The network itself is gone: no interface, no route, DNS resolution
    /// failing outright, an OS-level "network unreachable" signal, or an
    /// established connection dropping in a way that looks like the network
    /// disappeared rather than the server objecting to something. Always
    /// moves to [`ConnectionState::Offline`], from any prior state,
    /// unconditionally — "we now know for certain the network is down"
    /// overrides whatever backoff bookkeeping [`ConnectionState::Reconnecting`]
    /// was carrying, since that bookkeeping is about a completely different
    /// problem.
    NetworkLost,
    /// The network is back (an OS/NetworkManager signal, or a successful
    /// low-level reachability probe). Only meaningful from
    /// [`ConnectionState::Offline`]: it schedules an immediate first
    /// reconnect attempt (`next_attempt_at == now`, no backoff delay,
    /// because nothing has failed yet on this axis). From any other state
    /// it is a no-op — we're already connected or already mid-attempt, so a
    /// redundant "network is up" signal has nothing useful to do.
    NetworkRestored,
    /// A connect attempt failed, or a live connection broke, for a reason
    /// that is *not* "the network is gone" — auth rejection, a timeout with
    /// the network otherwise up, a malformed/unexpected server response, a
    /// connection dropped mid-IDLE while other traffic on the same link is
    /// fine. Bumps `consecutive_failures` and schedules the next attempt via
    /// [`backoff_floor_secs`]. From [`ConnectionState::Offline`] this is a
    /// no-op: we already know the problem is "no network", and a stray
    /// failure report from some in-flight attempt must not be allowed to
    /// relabel that as "server problem, keep backing off" — see the module
    /// doc comment on why the two axes must stay separate.
    ConnectFailed,
    /// A connect attempt succeeded (login/select ok, or an IDLE round-trip
    /// completed). Always moves to [`ConnectionState::Online`] and resets
    /// the failure count, regardless of the prior state.
    ConnectSucceeded,
}

/// D32 backoff table — see the module doc comment for why this is a
/// deliberate, documented duplicate of `feathermail_core::retry_delay_secs`
/// and `crate::schedule::backoff_floor_secs` rather than a shared call.
fn backoff_floor_secs(consecutive_failures: u32) -> i64 {
    crate::backoff::backoff_delay_secs(consecutive_failures)
}

/// Advance the state machine by one event. Pure function of `(state, event,
/// now)` — no hidden state, no I/O, so a caller can replay any sequence of
/// events deterministically in a test (see below) exactly as it would
/// happen against a real, flaky network.
pub fn transition(state: ConnectionState, event: ConnectionEvent, now: i64) -> ConnectionState {
    match event {
        ConnectionEvent::NetworkLost => ConnectionState::Offline,
        ConnectionEvent::NetworkRestored => match state {
            ConnectionState::Offline => ConnectionState::Reconnecting {
                consecutive_failures: 0,
                next_attempt_at: now,
            },
            other => other,
        },
        ConnectionEvent::ConnectFailed => match state {
            // We already know the network is down; a stray failure report
            // changes nothing (see the doc comment on this variant).
            ConnectionState::Offline => ConnectionState::Offline,
            ConnectionState::Reconnecting {
                consecutive_failures,
                ..
            } => {
                let failures = consecutive_failures.saturating_add(1);
                ConnectionState::Reconnecting {
                    consecutive_failures: failures,
                    next_attempt_at: now + backoff_floor_secs(failures),
                }
            }
            ConnectionState::Online => ConnectionState::Reconnecting {
                consecutive_failures: 1,
                next_attempt_at: now + backoff_floor_secs(1),
            },
        },
        ConnectionEvent::ConnectSucceeded => ConnectionState::Online,
    }
}

/// Is another connect attempt due right now? `false` for [`ConnectionState::Online`]
/// (nothing to attempt — already connected) and for [`ConnectionState::Offline`]
/// (no network to dial against; waiting on an external signal, not a timer,
/// is the entire point of that state). Only [`ConnectionState::Reconnecting`]
/// has an internal clock, and only once it has passed `next_attempt_at`.
pub fn due_now(state: ConnectionState, now: i64) -> bool {
    match state {
        ConnectionState::Online | ConnectionState::Offline => false,
        ConnectionState::Reconnecting {
            next_attempt_at, ..
        } => now >= next_attempt_at,
    }
}

/// Seconds until the next scheduled action, or `None` if there isn't one to
/// wait on: [`ConnectionState::Online`] has nothing to schedule, and
/// [`ConnectionState::Offline`] deliberately has no timer at all (see the
/// module doc comment) — a caller in that state should sleep until the next
/// external network-restored signal, not on a duration this function hands
/// back.
pub fn next_wakeup_secs(state: ConnectionState, now: i64) -> Option<i64> {
    match state {
        ConnectionState::Online | ConnectionState::Offline => None,
        ConnectionState::Reconnecting {
            next_attempt_at, ..
        } => Some((next_attempt_at - now).max(0)),
    }
}

/// Should the operation queue and IDLE be running? Only once actually
/// [`ConnectionState::Online`] — this is the "resume queue + idle" half of
/// the ticket's artifact, expressed as a query over the state rather than a
/// separate signal, so it can never drift out of sync with what `transition`
/// decided.
pub fn should_resume_queue_and_idle(state: ConnectionState) -> bool {
    matches!(state, ConnectionState::Online)
}

/// Did this transition just bring the connection back online from an
/// outage (offline or reconnecting), as opposed to it having already been
/// online? Useful for a caller that wants to fire a one-time "resume the
/// queue and re-enter IDLE" action exactly on the edge, rather than
/// re-issuing it on every tick that happens to observe `Online`.
pub fn resumed_from_outage(previous: ConnectionState, next: ConnectionState) -> bool {
    !matches!(previous, ConnectionState::Online) && matches!(next, ConnectionState::Online)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ten consecutive failed connect attempts, each recorded only once the
    /// machine itself says an attempt is due — mirroring exactly how a
    /// driver loop is required to use this module — must not produce ten
    /// immediate (`due_now == true` at the same `now`) reconnects. Only the
    /// very first attempt (before any backoff exists) may fire without
    /// delay; every subsequent one must be held back, and the held-back
    /// delay must grow along the documented D32 table.
    #[test]
    fn ten_consecutive_failures_do_not_storm_and_delays_grow() {
        let mut state = ConnectionState::Online;
        let mut now = 0i64;
        let expected_delays = [2, 5, 15, 30, 60, 120, 240, 480, 900, 900];

        for (attempt, expected_delay) in expected_delays.iter().enumerate() {
            // The very first failure (attempt 0) can come from a live
            // connection breaking at any moment — there is no schedule to
            // be "due" against yet, since we start `Online`, not
            // `Reconnecting`. From attempt 1 onward we are always
            // `Reconnecting`, and every one of those *must* be gated by
            // `due_now` — this is the actual storm check: the driver loop
            // is only allowed to record another `ConnectFailed` once the
            // machine itself says an attempt is due.
            if attempt > 0 {
                assert!(
                    due_now(state, now),
                    "attempt {attempt}: expected to be due at now={now}, state={state:?}"
                );
            }
            let before = state;
            state = transition(state, ConnectionEvent::ConnectFailed, now);
            let ConnectionState::Reconnecting {
                next_attempt_at,
                consecutive_failures,
            } = state
            else {
                panic!("expected Reconnecting after a failure, got {state:?}");
            };
            assert_eq!(consecutive_failures, attempt as u32 + 1);
            assert_eq!(
                next_attempt_at - now,
                *expected_delay,
                "attempt {attempt}: wrong backoff delay coming from {before:?}"
            );

            // Not due again immediately: this is the storm check. Advance
            // right up to (but not past) the deadline and confirm it still
            // isn't due, then land exactly on it.
            assert!(
                !due_now(state, next_attempt_at - 1),
                "attempt {attempt}: fired one second early"
            );
            now = next_attempt_at;
        }
    }

    /// After a success, backoff must fully reset — a folder/connection that
    /// climbed to a large failure count and then finally succeeded must not
    /// have its *next* failure treated as failure #11 (tiny delay skipped
    /// straight to the ceiling would be one bug; the opposite — never
    /// resetting — would defeat "recovers gracefully").
    #[test]
    fn success_resets_backoff() {
        let mut state = ConnectionState::Online;
        let mut now = 0i64;
        for _ in 0..8 {
            state = transition(state, ConnectionEvent::ConnectFailed, now);
            let ConnectionState::Reconnecting {
                next_attempt_at, ..
            } = state
            else {
                unreachable!()
            };
            now = next_attempt_at;
        }
        let ConnectionState::Reconnecting {
            consecutive_failures,
            ..
        } = state
        else {
            unreachable!()
        };
        assert!(consecutive_failures >= 8);

        state = transition(state, ConnectionEvent::ConnectSucceeded, now);
        assert_eq!(state, ConnectionState::Online);
        assert!(should_resume_queue_and_idle(state));

        // The very next failure after a success must be treated as failure
        // #1 again (2s delay), not a continuation of the old streak.
        state = transition(state, ConnectionEvent::ConnectFailed, now);
        let ConnectionState::Reconnecting {
            consecutive_failures,
            next_attempt_at,
        } = state
        else {
            panic!("expected Reconnecting, got {state:?}");
        };
        assert_eq!(consecutive_failures, 1);
        assert_eq!(
            next_attempt_at - now,
            2,
            "backoff must restart at the floor after a success"
        );
    }

    /// The headline artifact from the ticket: network drops (mid-backoff,
    /// even), the UI/queue must reflect Offline immediately, and once the
    /// network comes back the queue/IDLE resume — the state must not get
    /// stuck in Reconnecting waiting for a backoff timer that will never do
    /// anything, since there's no server to blame while there's no network.
    #[test]
    fn network_lost_then_restored_resumes_queue_and_idle() {
        let mut now = 0i64;
        let mut state = ConnectionState::Online;
        assert!(should_resume_queue_and_idle(state));

        // Network drops while we happen to already be backing off from a
        // server error, to prove NetworkLost overrides that bookkeeping.
        state = transition(state, ConnectionEvent::ConnectFailed, now);
        state = transition(state, ConnectionEvent::ConnectFailed, now);
        assert!(matches!(state, ConnectionState::Reconnecting { .. }));

        state = transition(state, ConnectionEvent::NetworkLost, now);
        assert_eq!(state, ConnectionState::Offline);
        assert!(!should_resume_queue_and_idle(state));
        assert!(!due_now(state, now));
        assert_eq!(next_wakeup_secs(state, now), None);

        // No amount of waiting brings it back on its own: Offline has no
        // internal clock, by design.
        assert!(!due_now(state, now + 10_000_000));

        // Network comes back: schedule an immediate first attempt, no
        // backoff delay (nothing has failed on *this* axis yet).
        now += 300;
        let before = state;
        state = transition(state, ConnectionEvent::NetworkRestored, now);
        assert!(
            due_now(state, now),
            "must be immediately due, not backed off"
        );
        assert!(!resumed_from_outage(before, state), "not Online yet");

        // The actual reconnect attempt succeeds.
        state = transition(state, ConnectionEvent::ConnectSucceeded, now);
        assert_eq!(state, ConnectionState::Online);
        assert!(should_resume_queue_and_idle(state));
        assert!(resumed_from_outage(before, state));
        assert!(
            !matches!(state, ConnectionState::Reconnecting { .. }),
            "must not be stuck in Reconnecting after a successful resume"
        );
    }

    /// A stray `ConnectFailed` while genuinely `Offline` must not be
    /// reinterpreted as "server problem" — it stays `Offline`, still with
    /// no internal clock, rather than starting a backoff countdown against
    /// a network that plainly isn't there.
    #[test]
    fn stray_connect_failed_while_offline_is_a_no_op() {
        let state = ConnectionState::Offline;
        let next = transition(state, ConnectionEvent::ConnectFailed, 1_000);
        assert_eq!(next, ConnectionState::Offline);
        assert!(!due_now(next, 1_000));
        assert!(!due_now(next, 999_999_999));
    }

    /// A redundant `NetworkRestored` while already fully `Online` (e.g. two
    /// independent NetworkManager signals) must not perturb anything —
    /// still connected, no reconnect attempt gets scheduled out of nowhere.
    #[test]
    fn redundant_network_restored_while_online_is_a_no_op() {
        let state = ConnectionState::Online;
        let next = transition(state, ConnectionEvent::NetworkRestored, 5_000);
        assert_eq!(next, ConnectionState::Online);
    }

    /// Likewise while already `Reconnecting`: a `NetworkRestored` signal
    /// must not erase the in-progress backoff schedule (the problem here
    /// was never "no network" in the first place, so nothing should reset).
    #[test]
    fn redundant_network_restored_while_reconnecting_is_a_no_op() {
        let state = transition(ConnectionState::Online, ConnectionEvent::ConnectFailed, 0);
        let next = transition(state, ConnectionEvent::NetworkRestored, 3);
        assert_eq!(
            next, state,
            "must not disturb an in-progress server backoff"
        );
    }

    #[test]
    fn due_now_false_before_deadline_true_at_and_after() {
        let state = transition(ConnectionState::Online, ConnectionEvent::ConnectFailed, 100);
        let ConnectionState::Reconnecting {
            next_attempt_at, ..
        } = state
        else {
            unreachable!()
        };
        assert!(!due_now(state, next_attempt_at - 1));
        assert!(due_now(state, next_attempt_at));
        assert!(due_now(state, next_attempt_at + 500));
    }

    #[test]
    fn backoff_never_exceeds_the_fifteen_minute_ceiling_even_at_huge_failure_counts() {
        let mut state = ConnectionState::Reconnecting {
            consecutive_failures: 10_000,
            next_attempt_at: 0,
        };
        state = transition(state, ConnectionEvent::ConnectFailed, 0);
        let ConnectionState::Reconnecting {
            next_attempt_at, ..
        } = state
        else {
            unreachable!()
        };
        assert_eq!(
            next_attempt_at,
            15 * 60,
            "no reconnect storm even at absurd failure counts"
        );
    }
}
