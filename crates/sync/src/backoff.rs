//! The one retry/backoff table (D32).
//!
//! 2, 5, 15, 30, 60 seconds, then doubling, capped at 15 minutes.
//!
//! It lives here, in the dependency-free crate, because three separate
//! places need it and they cannot all reach each other: the operation
//! queue in `feathermail-core` (a failed IMAP write), the folder
//! scheduler in [`crate::schedule`] (a folder that keeps failing to sync)
//! and the connection machine in [`crate::connection`] (a server that
//! keeps refusing to connect). `feathermail-core` depends on this crate,
//! so it can delegate here; the reverse is not possible and must not
//! become possible -- see the crate docs.
//!
//! Three hand-kept copies is how a table like this drifts: someone
//! raises the ceiling in one place, the other two keep the old one, and
//! the reconnect storm D32 exists to prevent comes back through the copy
//! nobody edited.

/// Seconds to wait after `consecutive_failures` failures in a row.
/// Zero failures means no delay at all.
pub fn backoff_delay_secs(consecutive_failures: u32) -> i64 {
    match consecutive_failures {
        0 => 0,
        1 => 2,
        2 => 5,
        3 => 15,
        4 => 30,
        5 => 60,
        n => {
            let shift = n.saturating_sub(5).min(20);
            60i64.saturating_mul(1i64 << shift).min(15 * 60)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::backoff_delay_secs;

    #[test]
    fn matches_the_d32_table_exactly() {
        let table = [(0, 0), (1, 2), (2, 5), (3, 15), (4, 30), (5, 60)];
        for (failures, expected) in table {
            assert_eq!(
                backoff_delay_secs(failures),
                expected,
                "failures={failures}"
            );
        }
        // Then doubling from 60s...
        assert_eq!(backoff_delay_secs(6), 120);
        assert_eq!(backoff_delay_secs(7), 240);
        assert_eq!(backoff_delay_secs(8), 480);
        // ...until the 15-minute ceiling, which nothing may exceed.
        assert_eq!(backoff_delay_secs(9), 900);
        assert_eq!(backoff_delay_secs(50), 900);
        assert_eq!(backoff_delay_secs(u32::MAX), 900);
    }
}
