//! T-133: the one door every settings write goes through, and the reason
//! it exists.
//!
//! SQLite serialises writers. While the backfill worker is committing a
//! batch of headers (`CoreSyncStore::upsert_headers` — one transaction per
//! batch, and on a 200k-message folder its rollup is not cheap), any other
//! write on the same file waits — up to `busy_timeout`, which
//! `feathermail_db` sets to 5000 ms. A settings write on the GTK thread
//! therefore parks the whole window for up to five seconds; the owner's
//! journal shows exactly one such stall, 5012.8 ms, on the click that
//! turned remote images on. That click writes `allowed_image_domains`.
//!
//! So no settings write happens on the GTK thread any more. A patch is
//! handed to this writer, which owns a `Core` handle of its own (a third
//! connection, next to `core` and `reader` — see `open_core_handles`) and
//! applies patches in the order they were sent. The GTK thread never
//! blocks: the send is a channel push. What the writer waits on is its own
//! business.
//!
//! Two properties the callers rely on:
//!
//! * **Order.** One thread, one queue: two patches to the same key land in
//!   the order the user made them, so the last click wins on disk exactly
//!   as it does on screen.
//! * **Coalescing.** Everything already queued is applied under a single
//!   flush, so dragging a divider (one patch per motion event) costs one
//!   transaction per drain, not one per event.
//!
//! `SettingsStore::flush` writes only the keys whose value actually
//! changed, so this handle's older copy of a key another process owns
//! never overwrites it.

use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};

use feathermail_core::{Core, Settings};

type Patch = Box<dyn FnOnce(&mut Settings) + Send + 'static>;

/// Where the writer thread puts the settings it is handed.
// `Own` carries a whole `Core`; `Shared` an `Arc`. The size difference is
// the point (one owns its handle, the other borrows the window's), and
// exactly one of these is built per process.
#[allow(clippy::large_enum_variant)]
pub enum SettingsSink {
    /// The normal case: a `Core` handle of the writer's own, on the same
    /// on-disk profile. Nothing on the GTK thread can be blocked by it.
    Own(Core),
    /// The ephemeral session (`Core::memory()`): there is only one handle
    /// and one in-memory database, so the writer shares it. Safe — an
    /// in-memory profile has no background worker writing to it, which is
    /// the same reason `open_core_handles` lets `core` and `reader` be the
    /// same handle there.
    Shared(Arc<Mutex<Core>>),
}

/// A handle to the settings writer thread. Cloneable; dropping every clone
/// closes the channel and ends the thread.
#[derive(Clone)]
pub struct SettingsWriter {
    tx: Sender<Patch>,
}

impl SettingsWriter {
    pub fn spawn(sink: SettingsSink) -> Self {
        let (tx, rx) = mpsc::channel::<Patch>();
        std::thread::Builder::new()
            .name("settings-writer".into())
            .spawn(move || {
                let mut sink = sink;
                while let Ok(first) = rx.recv() {
                    let mut batch: Vec<Patch> = vec![first];
                    // Drain whatever else is already queued: a drag emits
                    // a patch per motion event and they must not each buy
                    // their own transaction.
                    while let Ok(next) = rx.try_recv() {
                        batch.push(next);
                    }
                    sink.apply(batch);
                }
            })
            .expect("the settings writer thread must start");
        Self { tx }
    }

    /// Queues one settings patch. Returns immediately; the write happens on
    /// the writer thread. A closed channel (the thread is gone, which can
    /// only happen at shutdown) drops the patch rather than panicking on
    /// the GTK thread.
    pub fn write(&self, patch: impl FnOnce(&mut Settings) + Send + 'static) {
        let _ = self.tx.send(Box::new(patch));
    }
}

impl SettingsSink {
    fn apply(&mut self, batch: Vec<Patch>) {
        match self {
            SettingsSink::Own(core) => apply_to(core, batch),
            SettingsSink::Shared(core) => {
                let mut core = match core.lock() {
                    Ok(core) => core,
                    Err(poisoned) => poisoned.into_inner(),
                };
                apply_to(&mut core, batch);
            }
        }
    }
}

/// How many times a flush is retried before the patch is reported lost.
/// Each attempt already waits out SQLite's own `busy_timeout` (5 s), so
/// this is the answer to "the backfill was mid-batch for longer than
/// that", not a spin.
const FLUSH_ATTEMPTS: usize = 3;
const RETRY_PAUSE: std::time::Duration = std::time::Duration::from_millis(200);

fn apply_to(core: &mut Core, batch: Vec<Patch>) {
    let now_ms = now_ms();
    for patch in batch {
        core.patch_settings(now_ms, patch);
    }
    // A settings segment is one click, not a keystroke: flush now rather
    // than leaving it to the 750 ms autosave, so a quit right after the
    // click still keeps the choice. The wait this can incur is the whole
    // point of being on this thread.
    //
    // T-135: a failed flush used to be discarded (`let _ =` on the GTK
    // thread), which is how "the merged view was open when I closed it"
    // became "it opens on one account again". The patch is still in the
    // store's dirty state, so a retry writes exactly the same thing.
    for attempt in 1..=FLUSH_ATTEMPTS {
        match core.flush_settings() {
            Ok(()) => return,
            Err(err) if attempt == FLUSH_ATTEMPTS => {
                eprintln!("feathermail: could not write settings: {}", err.message);
            }
            Err(_) => std::thread::sleep(RETRY_PAUSE),
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Order and coalescing are the two promises the callers make on the
    /// GTK thread; both are visible from the outside as "the last patch
    /// wins, and it is on disk".
    #[test]
    fn patches_land_in_the_order_they_were_sent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        let writer = SettingsWriter::spawn(SettingsSink::Own(Core::open(&path).unwrap()));
        writer.write(|s| s.ui_scale_percent = 110);
        writer.write(|s| s.ui_scale_percent = 125);
        writer.write(|s| s.last_unified = true);
        drop(writer);
        // The thread ends when the channel closes, but only after it has
        // drained it; poll the file instead of sleeping a fixed time.
        let mut settings = None;
        for _ in 0..200 {
            let core = Core::open(&path).unwrap();
            if core.settings().last_unified {
                settings = Some(core.settings().clone());
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let settings = settings.expect("the writer must reach disk");
        assert_eq!(settings.ui_scale_percent, 125, "the last patch wins");
        assert!(settings.last_unified);
    }

    #[test]
    fn a_write_never_blocks_the_caller_on_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        let writer = SettingsWriter::spawn(SettingsSink::Own(Core::open(&path).unwrap()));
        let started = std::time::Instant::now();
        for _ in 0..200 {
            writer.write(|s| s.last_unified = true);
        }
        assert!(
            started.elapsed() < std::time::Duration::from_millis(50),
            "queueing settings writes must be a channel push, not a disk wait"
        );
    }
}
