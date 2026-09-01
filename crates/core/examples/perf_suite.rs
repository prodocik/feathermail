//! Local synthetic Core performance suite for T-068.
//!
//! The input is a disposable database produced by `generate_dataset`. This
//! measures Core/SQLite work, not GTK painting or compositor scrolling; the
//! metric names deliberately say `page` rather than claiming a UI scroll.

use std::env;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use feathermail_core::body::BodyLookup;
use feathermail_core::{
    AccountId, Command, Core, FolderId, ListThreadsQuery, ThreadFilter, DEFAULT_INDEX_BATCH,
    LIST_PAGE,
};
use feathermail_db::{Database, SCHEMA_VERSION};
use feathermail_search::Query;

const HOT_PATH_BUDGET: Duration = Duration::from_millis(100);
const BULK_ARCHIVE_SIZE: usize = 32;

#[derive(Clone, Copy)]
struct Metric {
    name: &'static str,
    elapsed: Duration,
    /// `true` only where §61 has a concrete sub-100-ms target. Bulk archive
    /// is reported but has no invented numeric threshold: the contract says
    /// "visually immediate", and GTK presentation is outside this runner.
    enforce_budget: bool,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (path, count) = parse_args()?;
    if !path.is_file() {
        return Err(format!("dataset is not a file: {}", path.display()).into());
    }
    if !matches!(count, 1_000 | 10_000 | 50_000 | 100_000) {
        return Err("COUNT must be 1000, 10000, 50000, or 100000".into());
    }

    let bodies_dir = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("bodies");
    // T-070: the app pays this before it can paint anything, and the
    // startup benchmark showed a profile with mail costs ~200 ms more to
    // reach a visible Inbox than an empty one. Timing the open separates
    // "the profile was expensive to open" from "the first page was
    // expensive to list", which the metrics below already cover.
    let (mut core, profile_open) = timed(|| Core::open(&path))?;
    let account_id = AccountId("perf".into());
    let inbox_id = FolderId("perf:inbox".into());
    let archive_id = FolderId("perf:archive".into());
    let expected_inbox = count - count / 4;
    let expected_archive = count / 4;

    let (inbox_page, inbox_first_page) =
        timed(|| core.list_threads(list_query(&account_id, &inbox_id, None)))?;
    if inbox_page.total != expected_inbox || inbox_page.threads.len() != LIST_PAGE {
        return Err(format!(
            "dataset inbox is not the expected first page: total={}, rows={}",
            inbox_page.total,
            inbox_page.threads.len()
        )
        .into());
    }
    let next = inbox_page
        .next
        .clone()
        .ok_or("synthetic inbox needs a second page")?;
    let (_, inbox_next_page) =
        timed(|| core.list_threads(list_query(&account_id, &inbox_id, Some(next))))?;

    let opened_id = inbox_page.threads[0]
        .message_id
        .as_ref()
        .ok_or("synthetic inbox row has no message id")?
        .clone();
    let (_, open_thread) = timed(|| core.get_thread(&account_id, &inbox_page.threads[0].id))?;
    let (cached_body, open_cached_body) = timed(|| core.lookup_body(&opened_id, &bodies_dir))?;
    if !matches!(cached_body, BodyLookup::Cached(_)) {
        return Err("synthetic first inbox message is not cached".into());
    }

    let (archive_page, folder_switch) =
        timed(|| core.list_threads(list_query(&account_id, &archive_id, None)))?;
    if archive_page.total != expected_archive || archive_page.threads.is_empty() {
        return Err(format!(
            "dataset archive is not the expected folder: total={}",
            archive_page.total
        )
        .into());
    }

    let index_started = Instant::now();
    let mut indexed = 0usize;
    loop {
        let batch = core.index_pending_batch(&bodies_dir, DEFAULT_INDEX_BATCH)?;
        indexed += batch.indexed;
        if batch.remaining == 0 {
            break;
        }
    }
    let initial_index = index_started.elapsed();
    if indexed != count {
        return Err(format!("indexed {indexed} messages, expected {count}").into());
    }

    let plan = Query::parse("Synthetic message").to_search_plan();
    let (search_page, search) = timed(|| core.search(&account_id, &plan, None, LIST_PAGE))?;
    if search_page.threads.len() != LIST_PAGE || search_page.pending_index != 0 {
        return Err(format!(
            "synthetic search is incomplete: rows={}, pending_index={}",
            search_page.threads.len(),
            search_page.pending_index
        )
        .into());
    }

    let archive_ids = inbox_page
        .threads
        .iter()
        .take(BULK_ARCHIVE_SIZE)
        .map(|thread| thread.id.clone())
        .collect::<Vec<_>>();
    let (_, bulk_archive) = timed(|| {
        core.dispatch(Command::Archive {
            account_id: account_id.clone(),
            thread_ids: archive_ids.clone(),
        })
    })?;
    let inbox_after = core.list_threads(list_query(&account_id, &inbox_id, None))?;
    if inbox_after.total != expected_inbox - archive_ids.len() {
        return Err(format!(
            "bulk archive did not update the inbox synchronously: total={}",
            inbox_after.total
        )
        .into());
    }

    // `core-suite.sh` creates a disposable generated profile. Reopen that
    // already-caught-up synthetic fixture as v18, then time the complete
    // current v18-to-v19 upgrade: schema creation plus the rowid-map backfill.
    // This is deliberately not a misleading aggregate for every historical
    // schema upgrade. Drop Core first so the benchmark opens its own SQLite
    // handle.
    drop(core);
    let v19_upgrade = measure_v19_upgrade(&path, count)?;

    let metrics = [
        Metric {
            // Reported, not enforced: §61's 100 ms target is about
            // interactions with a shell that is already up. What bounds this
            // one is the startup budget in `scripts/perf/startup.py`, and
            // that budget covers process launch and GTK too.
            name: "profile_open",
            elapsed: profile_open,
            enforce_budget: false,
        },
        Metric {
            name: "inbox_first_page",
            elapsed: inbox_first_page,
            enforce_budget: true,
        },
        Metric {
            name: "inbox_next_page",
            elapsed: inbox_next_page,
            enforce_budget: true,
        },
        Metric {
            name: "open_thread_metadata",
            elapsed: open_thread,
            enforce_budget: true,
        },
        Metric {
            name: "open_cached_body",
            elapsed: open_cached_body,
            enforce_budget: true,
        },
        Metric {
            name: "folder_switch",
            elapsed: folder_switch,
            enforce_budget: true,
        },
        Metric {
            name: "initial_index",
            elapsed: initial_index,
            enforce_budget: false,
        },
        Metric {
            name: "v19_upgrade",
            elapsed: v19_upgrade,
            enforce_budget: false,
        },
        Metric {
            name: "search",
            elapsed: search,
            enforce_budget: true,
        },
        Metric {
            name: "bulk_archive_32",
            elapsed: bulk_archive,
            enforce_budget: false,
        },
    ];
    print_summary(count, &metrics);
    for metric in metrics {
        if metric.enforce_budget && metric.elapsed > HOT_PATH_BUDGET {
            return Err(format!(
                "{} took {:.1} ms, above the §61 100 ms target",
                metric.name,
                milliseconds(metric.elapsed)
            )
            .into());
        }
    }
    Ok(())
}

fn parse_args() -> Result<(PathBuf, usize), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    let path = args
        .next()
        .map(PathBuf::from)
        .ok_or("usage: perf_suite DATASET_PATH COUNT")?;
    let count: usize = args.next().ok_or("missing COUNT")?.parse()?;
    if args.next().is_some() {
        return Err("usage: perf_suite DATASET_PATH COUNT".into());
    }
    Ok((path, count))
}

fn list_query(
    account_id: &AccountId,
    folder_id: &FolderId,
    after: Option<feathermail_core::ThreadCursor>,
) -> ListThreadsQuery {
    ListThreadsQuery {
        account_id: account_id.clone(),
        folder_id: folder_id.clone(),
        filter: ThreadFilter::All,
        after,
        limit: LIST_PAGE,
    }
}

fn timed<T>(
    operation: impl FnOnce() -> Result<T, feathermail_core::CoreError>,
) -> Result<(T, Duration), feathermail_core::CoreError> {
    let started = Instant::now();
    let result = operation()?;
    Ok((result, started.elapsed()))
}

/// Turn an already-indexed synthetic v19 fixture back into the precise v18
/// state a current upgrade expects, then time the reopen that recreates the
/// schema artifacts and reconstitutes its rowid map. The guard means this
/// cannot silently benchmark an arbitrary partial profile: generated fixtures
/// have exactly one current migration marker, one FTS row per requested
/// synthetic message, and one map row per FTS row before the simulation starts.
fn measure_v19_upgrade(
    path: &Path,
    expected_messages: usize,
) -> Result<Duration, Box<dyn std::error::Error>> {
    let db = Database::open(path)?;
    let migration_rows: i64 =
        db.conn()
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| {
                row.get(0)
            })?;
    let indexed_rows: i64 =
        db.conn()
            .query_row("SELECT COUNT(*) FROM messages_fts", [], |row| row.get(0))?;
    let mapped_rows: i64 =
        db.conn()
            .query_row("SELECT COUNT(*) FROM fts_message_rows", [], |row| {
                row.get(0)
            })?;
    let synthetic_accounts: i64 = db.conn().query_row(
        "SELECT COUNT(*) FROM accounts WHERE id = 'perf' AND email = 'perf@example.invalid'",
        [],
        |row| row.get(0),
    )?;
    let all_accounts: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM accounts", [], |row| row.get(0))?;
    if db.schema_version()? != SCHEMA_VERSION
        || migration_rows != 1
        || indexed_rows != expected_messages as i64
        || mapped_rows != indexed_rows
        || synthetic_accounts != 1
        || all_accounts != 1
    {
        return Err("v19 upgrade benchmark requires a complete generated fixture".into());
    }

    // A genuine v18 profile has neither the v19 rowid-map table nor the
    // account-wide FTS ordering index. Remove both before resetting the
    // marker: `Database::open` will recreate them from schema.sql and then
    // run the map backfill, exactly in production order. The guard above
    // makes this destructive simulation exclusive to a generated fixture.
    db.conn().execute("DROP TABLE fts_message_rows", [])?;
    db.conn().execute("DROP INDEX threads_account_date", [])?;
    db.conn().execute("DELETE FROM schema_migrations", [])?;
    db.conn().execute(
        "INSERT INTO schema_migrations (version, applied_at) VALUES (18, 0)",
        [],
    )?;
    drop(db);

    let started = Instant::now();
    let migrated = Database::open(path)?;
    let elapsed = started.elapsed();
    let after: i64 =
        migrated
            .conn()
            .query_row("SELECT COUNT(*) FROM fts_message_rows", [], |row| {
                row.get(0)
            })?;
    if migrated.schema_version()? != SCHEMA_VERSION || after != indexed_rows {
        return Err("v19 upgrade did not restore every FTS rowid map entry".into());
    }
    Ok(elapsed)
}

fn milliseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

fn print_summary(count: usize, metrics: &[Metric]) {
    print!(
        "{{\"messages\":{count},\"scope\":\"Core/SQLite only; not GTK paint or compositor scroll\",\"hot_path_budget_ms\":100.0,\"metrics_ms\":{{"
    );
    for (index, metric) in metrics.iter().enumerate() {
        if index > 0 {
            print!(",");
        }
        print!("\"{}\":{:.1}", metric.name, milliseconds(metric.elapsed));
    }
    println!("}}}}");
}
