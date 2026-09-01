//! T-092: fail-closed proof that `crates/app` never calls
//! [`feathermail_core::Core::index_pending_batch`] itself.
//!
//! D11 (background work never runs on the GTK thread) is why this matters:
//! draining `fts_pending` means at least one SQLite write transaction plus,
//! per row, a cached body file read off disk (`crates/core/src/search.rs`'s
//! `index_one`/`body_text_for_index`) -- unbounded disk I/O the GTK thread
//! has no business doing. `crates/service/src/worker.rs`'s
//! `drain_one_index_batch` is meant to be the *only* call site in the
//! entire workspace; this test is what stands behind that claim once the
//! code is written, rather than leaving it as an assertion in a doc
//! comment nobody re-checks.
//!
//! ## What this test is, and is not
//!
//! This is a plain source-text search for the literal identifier
//! `index_pending_batch` across every `.rs` file under `crates/app/src`.
//! It is deliberately **allowlist-shaped, not blacklist-shaped**: rather
//! than grepping for a list of suspicious-sounding words (T-049 hit
//! exactly that failure mode -- a blacklist of words was defeated by one
//! line of GTK-thread code that reached into the database anyway, and the
//! test stayed green because the offending line simply didn't contain any
//! of the listed words), this checks for the *one* symbol that is
//! structurally required for `crates/app` to reach this capability at
//! all: there is no other name, alias, or code path through which
//! `Core::index_pending_batch` could be invoked without that exact
//! identifier appearing in the calling source. Zero tolerance on that one
//! name, not a heuristic scored against many.
//!
//! What it does **not** prove, honestly: Rust's visibility system has no
//! "`pub` to `feathermail-service` only, not to `feathermail-app`" level
//! -- both are ordinary external crates to `feathermail-core`, so
//! `index_pending_batch` being `pub` (required for `crates/service` to
//! call it at all) makes it just as reachable, at the type-checker level,
//! from `crates/app`. There is no compiler-enforced boundary this test
//! could lean on instead; a structural (type-system) proof was
//! considered and rejected for exactly that reason -- Rust simply does not
//! have the visibility level this would need. This test also cannot catch
//! indirection designed specifically to dodge it (a re-exported alias, a
//! macro that assembles the identifier at compile time, a `#[path]`
//! trick) -- nothing currently in this workspace does anything like that,
//! and inventing defenses against a hypothetical deliberately-evasive
//! future change is not this ticket's job. It catches exactly what T-049's
//! regression looked like: an ordinary, direct call written in by hand.

use std::path::{Path, PathBuf};

/// The one symbol this test exists to keep out of `crates/app`. Named
/// once so the assertion message and the search itself cannot drift
/// apart from each other.
const FORBIDDEN_SYMBOL: &str = "index_pending_batch";

/// T-134: the second one, drained by the same loop turn for the same
/// reason. Recomputing a queued snippet reads a cached body file per row
/// (`Core::repair_snippet_batch`), so `crates/app` must not reach it
/// either.
const FORBIDDEN_REPAIR_SYMBOL: &str = "repair_snippet_batch";

#[test]
fn crates_app_source_never_spells_out_index_pending_batch() {
    let app_src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../app/src");
    assert!(
        app_src.is_dir(),
        "expected {} to exist -- this test's own assumption about the workspace layout \
         (crates/service and crates/app as siblings) is stale, fix the path before trusting \
         a pass or a fail from this test",
        app_src.display()
    );

    let mut rs_files = Vec::new();
    collect_rs_files(&app_src, &mut rs_files);
    assert!(
        rs_files.len() >= 5,
        "found suspiciously few .rs files under {} ({}) -- this test would pass vacuously \
         against an empty or wrongly-pointed directory, which proves nothing (crates/app/src \
         has 6 .rs files as of T-092: main, msg, nav, rows, secret_store, shell)",
        app_src.display(),
        rs_files.len()
    );

    for symbol in [FORBIDDEN_SYMBOL, FORBIDDEN_REPAIR_SYMBOL] {
        let offenders: Vec<&PathBuf> = rs_files
            .iter()
            .filter(|path| {
                std::fs::read_to_string(path)
                    .unwrap_or_default()
                    .contains(symbol)
            })
            .collect();

        assert!(
            offenders.is_empty(),
            "D11: crates/app must never call {symbol} directly -- draining a queue of cached \
             bodies is unbounded local disk/SQLite work that belongs on the background worker \
             (crates/service/src/worker.rs) only. Found the symbol in: {offenders:?}"
        );
    }
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in
        std::fs::read_dir(dir).unwrap_or_else(|err| panic!("reading {}: {err}", dir.display()))
    {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// Sanity check on the test itself: [`FORBIDDEN_SYMBOL`] must actually be
/// the real name of a real, existing method -- otherwise this whole file
/// could have quietly started proving nothing the moment somebody renamed
/// it, and stayed green regardless. Exercises the symbol through its own
/// real call site, `feathermail_core::Core`, rather than assuming the
/// string is still accurate.
#[test]
fn forbidden_symbol_is_the_real_method_name_not_a_stale_string() {
    let core = feathermail_core::Core::memory().unwrap();
    let dir = tempfile::tempdir().unwrap();
    // Real call, real signature -- if `index_pending_batch` were ever
    // renamed, this line (not just the string constant above) would fail
    // to compile, forcing this test file to be updated in the same
    // change instead of silently drifting.
    let _ = core.index_pending_batch(dir.path(), 1);
    assert_eq!(FORBIDDEN_SYMBOL, "index_pending_batch");
    // T-134: same guarantee for the snippet repair queue.
    let _ = core.repair_snippet_batch(dir.path(), 1);
    assert_eq!(FORBIDDEN_REPAIR_SYMBOL, "repair_snippet_batch");
}
