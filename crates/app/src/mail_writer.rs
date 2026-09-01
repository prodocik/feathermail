//! T-139/T-140/T-143: the door mailbox writes go through so the window keeps
//! moving while they happen.
//!
//! The sibling of [`crate::settings_writer`], for the same reason and with
//! the same shape. SQLite serialises writers: while the sync worker is
//! committing a batch of headers, any other write on the profile waits --
//! up to `busy_timeout`, which `feathermail_db` sets to 5000 ms -- and a
//! write started on the GTK thread parks the whole window for that long.
//!
//! Two defects came out of doing it on the GTK thread anyway:
//!
//! * **T-139.** "выставлена настройка маркировать прочитанным моментально но
//!   выбираю некоторые письма и они не маркируются прочитанными, а некоторые
//!   реально сразу". Mark-read-on-select dispatched inline and threw the
//!   result away (`let _ = ...`). A dispatch that lost the race against the
//!   backfill came back `Conflict` after its five-second wait, the row was
//!   then repainted from a database that still said unread, and nothing
//!   anywhere said so. Whether a letter stayed bold was decided by what the
//!   sync happened to be doing at that instant -- exactly the "some do,
//!   some do not" the owner saw.
//! * **T-140.** "в all account нажал Mark all as read и все на долго
//!   зависло". `mark_unified_folder_read` walks every mailbox, and on the
//!   owner's profile that is 264 ms of `list_folders` before the write even
//!   starts -- measured on a copy of the live profile, idle. On the live one
//!   the write then queues behind the backfill.
//! * **T-143.** The same synchronous door remained behind Archive and
//!   Snooze. Under active sync Archive surfaced `Couldn't archive`, while a
//!   Snooze that lost the write race looked like a dead control.
//!
//! These writes hand the work to this writer, which owns a `Core` handle of
//! its own and retries a write that only failed because another writer held
//! the file. The GTK thread's part is a channel push and a repaint; what the
//! writer waits on is its own business, and the shell hears the outcome as
//! an ordinary `Msg`.

use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use feathermail_core::{Core, CoreError};

type Job = Box<dyn FnOnce(&mut Core) + Send + 'static>;

/// Where the writer thread does its work. Same two cases as
/// [`crate::settings_writer::SettingsSink`], for the same reasons.
// `Own` carries a whole `Core`; `Shared` an `Arc`. The size difference is
// the point, and exactly one of these is built per process.
#[allow(clippy::large_enum_variant)]
pub enum MailSink {
    /// The normal case: a `Core` handle of the writer's own, on the same
    /// on-disk profile, so nothing on the GTK thread waits on this lock.
    Own(Core),
    /// The ephemeral session (`Core::memory()`): one handle, one in-memory
    /// database, no background worker writing to it.
    Shared(Arc<Mutex<Core>>),
}

/// A handle to the mailbox writer thread. Cloneable; dropping every clone
/// closes the channel and ends the thread.
#[derive(Clone)]
pub struct MailWriter {
    tx: Sender<Job>,
}

impl MailWriter {
    pub fn spawn(sink: MailSink) -> Self {
        let (tx, rx) = mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("mail-writer".into())
            .spawn(move || {
                let mut sink = sink;
                // One thread, one queue: two commands the user issued in
                // order reach SQLite in that order. Nothing is coalesced --
                // unlike a settings patch, every command here is its own
                // durable intent with its own Undo ticket.
                while let Ok(job) = rx.recv() {
                    sink.run(job);
                }
            })
            .expect("the mail writer thread must start");
        Self { tx }
    }

    /// Queues one mailbox write. Returns immediately; a closed channel (the
    /// thread is gone, which can only happen at shutdown) drops the job
    /// rather than panicking on the GTK thread.
    pub fn run(&self, job: impl FnOnce(&mut Core) + Send + 'static) {
        let _ = self.tx.send(Box::new(job));
    }
}

impl MailSink {
    fn run(&mut self, job: Job) {
        match self {
            MailSink::Own(core) => job(core),
            MailSink::Shared(core) => {
                let mut core = match core.lock() {
                    Ok(core) => core,
                    Err(poisoned) => poisoned.into_inner(),
                };
                job(&mut core);
            }
        }
    }
}

/// How many times a write is retried before its failure is reported to the
/// reader. Each attempt has already waited out SQLite's own `busy_timeout`
/// (5 s), so this answers "the other writer was mid-batch for longer than
/// that" rather than spinning.
const WRITE_ATTEMPTS: usize = 3;
const RETRY_PAUSE: Duration = Duration::from_millis(200);

/// Run one Core write, retrying a failure that a second attempt can fix.
///
/// T-139: every SQLite error Core raises -- `SQLITE_BUSY` included -- comes
/// back as one `CoreError`, so there is nothing finer to match on here. A
/// command that is genuinely invalid fails all three times and its message
/// reaches the reader; one that merely lost the race succeeds on the retry,
/// which is the whole difference between a letter that gets marked read and
/// one that quietly does not.
pub fn with_retries<T>(
    core: &mut Core,
    mut op: impl FnMut(&mut Core) -> Result<T, CoreError>,
) -> Result<T, CoreError> {
    let mut last = None;
    for attempt in 1..=WRITE_ATTEMPTS {
        match op(core) {
            Ok(value) => return Ok(value),
            Err(err) => {
                last = Some(err);
                if attempt < WRITE_ATTEMPTS {
                    std::thread::sleep(RETRY_PAUSE);
                }
            }
        }
    }
    Err(last.expect("a failed loop leaves its error"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use feathermail_core::{AccountId, Command, ThreadId};

    /// The promise the GTK thread relies on: queueing is a channel push.
    #[test]
    fn a_write_never_blocks_the_caller_on_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        let writer = MailWriter::spawn(MailSink::Own(Core::open(&path).unwrap()));
        let started = std::time::Instant::now();
        for _ in 0..200 {
            writer.run(|core| {
                // A command against an account that does not exist: this is
                // about the queueing cost, not the write.
                let _ = core.dispatch(Command::MarkRead {
                    account_id: AccountId("nobody".into()),
                    thread_ids: vec![ThreadId("nothing".into())],
                });
            });
        }
        assert!(
            started.elapsed() < Duration::from_millis(50),
            "queueing mailbox writes must be a channel push, not a disk wait"
        );
    }

    /// Jobs run in the order they were queued, and every one of them runs:
    /// a mark-read that is dropped is the defect T-139 is about.
    #[test]
    fn every_job_runs_in_the_order_it_was_queued() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        let writer = MailWriter::spawn(MailSink::Own(Core::open(&path).unwrap()));
        let seen = Arc::new(Mutex::new(Vec::new()));
        for i in 0..50 {
            let seen = Arc::clone(&seen);
            writer.run(move |_| seen.lock().unwrap().push(i));
        }
        drop(writer);
        let mut done = Vec::new();
        for _ in 0..200 {
            done = seen.lock().unwrap().clone();
            if done.len() == 50 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(done, (0..50).collect::<Vec<i32>>());
    }

    /// A write that fails for a reason a retry cannot fix still fails, and
    /// it is tried the agreed number of times before it does.
    #[test]
    fn a_hopeless_write_is_retried_and_then_reported() {
        let dir = tempfile::tempdir().unwrap();
        let mut core = Core::open(dir.path().join("mail.db")).unwrap();
        let mut attempts = 0;
        let result: Result<(), CoreError> = with_retries(&mut core, |core| {
            attempts += 1;
            core.dispatch(Command::MarkRead {
                account_id: AccountId("nobody".into()),
                thread_ids: vec![ThreadId("nothing".into())],
            })
        });
        assert!(result.is_err(), "an unknown account cannot be marked read");
        assert_eq!(attempts, WRITE_ATTEMPTS);
    }
}
