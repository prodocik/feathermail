use std::env;
use std::path::{Path, PathBuf};
use std::time::Instant;

use feathermail_core::{Core, MessageId, RETHREAD_SETTINGS_KEY};
use feathermail_db::Database;
use rusqlite::params;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    let path = args
        .next()
        .map(PathBuf::from)
        .ok_or("usage: generate_dataset PATH COUNT")?;
    let count: usize = args.next().ok_or("missing COUNT")?.parse()?;
    if !matches!(count, 1_000 | 10_000 | 50_000 | 100_000) {
        return Err("COUNT must be 1000, 10000, 50000, or 100000".into());
    }
    if path.exists() {
        return Err(format!("refusing to overwrite existing dataset: {}", path.display()).into());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let db = Database::open(&path)?;
    let started = Instant::now();
    let tx = db.conn().unchecked_transaction()?;
    tx.execute("INSERT OR IGNORE INTO accounts (id,name,email,provider,status,download_policy,created_at,updated_at) VALUES ('perf','Performance','perf@example.invalid','generic','offline','recent',0,0)",[])?;
    tx.execute("INSERT OR IGNORE INTO folders (id,account_id,remote_id,name,kind) VALUES ('perf:inbox','perf','INBOX','Inbox','inbox')",[])?;
    tx.execute("INSERT OR IGNORE INTO folders (id,account_id,remote_id,name,kind) VALUES ('perf:archive','perf','Archive','Archive','archive')",[])?;
    // The generator creates already-grouped one-message threads. Mark the
    // one-shot legacy rethread migration complete, otherwise `Core::open`
    // would spend its startup time reprocessing this deliberately-current
    // fixture rather than exercising the persisted 1k/10k profile.
    tx.execute(
        "INSERT INTO settings (key, value) VALUES (?1, '1')",
        params![RETHREAD_SETTINGS_KEY],
    )?;
    {
        let mut thread_stmt = tx.prepare("INSERT INTO threads (id,account_id,folder_id,subject,snippet,date,unread,starred,has_attachment,message_count) VALUES (?1,'perf',?2,?3,?4,?5,?6,?7,?8,1)")?;
        let mut message_stmt = tx.prepare("INSERT INTO messages (id,account_id,thread_id,folder_id,provider_uid,message_id_header,date,sender_name,sender_email,recipients,subject,snippet,unread,starred,has_attachment,size_bytes) VALUES (?1,'perf',?2,?3,?4,?5,?6,?7,?8,'perf@example.invalid',?9,?10,?11,?12,?13,2048)")?;
        let mut pending_stmt =
            tx.prepare("INSERT INTO fts_pending (message_id,queued_at) VALUES (?1,0)")?;
        for n in 0..count {
            let thread_id = format!("perf:t:{n:06}");
            let message_id = format!("perf:m:{n:06}");
            let subject = format!("Synthetic message {n}");
            let preview = format!("Bounded preview for row {n}");
            let date = 2_000_000_000_i64 - n as i64;
            // Three quarters of the mail stays in Inbox, one quarter lives
            // in a real Archive mailbox. The suite can therefore time a
            // meaningful folder switch and a bulk archive without inventing
            // a second synthetic database shape.
            let folder_id = if n % 4 == 3 {
                "perf:archive"
            } else {
                "perf:inbox"
            };
            thread_stmt.execute(params![
                thread_id,
                folder_id,
                subject,
                preview,
                date,
                (n % 3 == 0) as i64,
                (n % 11 == 0) as i64,
                (n % 17 == 0) as i64
            ])?;
            message_stmt.execute(params![
                message_id,
                thread_id,
                folder_id,
                n as i64 + 1,
                format!("<perf-{n}@invalid>"),
                date,
                format!("Sender {n}"),
                format!("sender{n}@example.invalid"),
                subject,
                preview,
                (n % 3 == 0) as i64,
                (n % 11 == 0) as i64,
                (n % 17 == 0) as i64
            ])?;
            pending_stmt.execute(params![message_id])?;
        }
    }
    tx.commit()?;
    let populated_in = started.elapsed();
    drop(db);

    // One real cached RFC 822 body lets the suite cover the on-disk open
    // path without making every large metadata dataset carry 100k bodies.
    // The data is deliberately synthetic and uses the reserved .invalid
    // domain, so perf artifacts can never contain mailbox content.
    let bodies_dir = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("bodies");
    let reopen_started = Instant::now();
    let mut core = Core::open(&path)?;
    let reopened_in = reopen_started.elapsed();
    let cache_seed_started = Instant::now();
    core.store_body(
        &MessageId("perf:m:000000".into()),
        &bodies_dir,
        b"From: Perf Sender <sender0@example.invalid>\r\nSubject: Synthetic message 0\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nCached synthetic performance body.\r\n",
    )?;
    eprintln!(
        "generated {count} synthetic messages at {}: rows {:.2?}, reopen {:.2?}, cached-body seed {:.2?}",
        path.display(),
        populated_in,
        reopened_in,
        cache_seed_started.elapsed()
    );
    Ok(())
}
