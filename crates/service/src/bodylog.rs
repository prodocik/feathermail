//! Temporary instrumentation, worker side. See the app crate's twin module.
//! Silent unless `FEATHERMAIL_BODY_LOG` is set.

use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("FEATHERMAIL_BODY_LOG").is_some())
}

pub fn line(args: std::fmt::Arguments<'_>) {
    if !enabled() {
        return;
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    eprintln!(
        "[body {:>6}.{:03}] wrk  {args}",
        now.as_secs() % 100_000,
        now.subsec_millis()
    );
}

#[macro_export]
macro_rules! bodylog {
    ($($arg:tt)*) => { $crate::bodylog::line(format_args!($($arg)*)) };
}
