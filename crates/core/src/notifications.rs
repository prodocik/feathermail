//! New-mail notification queries. Message bodies never cross this API.

use feathermail_html::decode_encoded_words;
use rusqlite::params;

use crate::error::CoreError;
use crate::model::{AccountId, ThreadId};
use crate::store::{sql_err, Core};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationCandidate {
    pub thread_id: ThreadId,
    pub sender: String,
    pub subject: String,
    pub date: i64,
}

impl Core {
    pub fn notification_watermark(&self, account_id: &AccountId) -> Result<i64, CoreError> {
        self.require_account(account_id.as_str())?;
        self.db
            .conn()
            .query_row(
                "SELECT COALESCE(MAX(m.date), 0) FROM messages m \
                 JOIN folders f ON f.id=m.folder_id AND f.account_id=m.account_id \
                 WHERE m.account_id=?1 AND f.kind='inbox'",
                params![account_id.as_str()],
                |row| row.get(0),
            )
            .map_err(sql_err)
    }

    /// T-159: how many letters [`Self::notification_candidates`] would have
    /// to choose from, before the limit cuts anything.
    ///
    /// The same account, the same Inbox, the same `unread=1 AND date>?`
    /// -- only `COUNT(*)` instead of the rows, and deliberately without the
    /// `LIMIT`, because the question it answers is exactly the one the
    /// limit hides: is this a handful of letters or a burst? A tick that
    /// lands more mail than the shell is willing to post one by one is
    /// announced as a single summary instead, and that decision cannot be
    /// made from a list that has already been truncated to the limit.
    pub fn notification_candidate_count(
        &self,
        account_id: &AccountId,
        after: i64,
    ) -> Result<usize, CoreError> {
        self.require_account(account_id.as_str())?;
        let count: i64 = self
            .db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM messages m \
                 JOIN folders f ON f.id=m.folder_id AND f.account_id=m.account_id \
                 WHERE m.account_id=?1 AND f.kind='inbox' AND m.unread=1 AND m.date>?2",
                params![account_id.as_str(), after],
                |row| row.get(0),
            )
            .map_err(sql_err)?;
        Ok(count.max(0) as usize)
    }

    pub fn notification_candidates(
        &self,
        account_id: &AccountId,
        after: i64,
        limit: usize,
    ) -> Result<Vec<NotificationCandidate>, CoreError> {
        self.require_account(account_id.as_str())?;
        // `limit` has to cut the *oldest* unread mail, not the newest: a
        // burst bigger than the limit still has to surface every message
        // over several ticks, and the caller advances its watermark from
        // the dates actually returned here (core-domain-04). So the limit
        // is applied to a `date DESC` inner ordering -- keeping the newest
        // `limit` rows -- and the outer query only re-sorts that kept set
        // back to ascending for display/notification order.
        let mut stmt = self
            .db
            .conn()
            .prepare(
                "SELECT thread_id, sender, subject, date FROM ( \
                     SELECT m.thread_id AS thread_id, \
                            CASE WHEN m.sender_name='' THEN m.sender_email ELSE m.sender_name END AS sender, \
                            CASE WHEN m.subject='' THEN '(No subject)' ELSE m.subject END AS subject, \
                            m.date AS date, m.id AS id \
                     FROM messages m \
                     JOIN folders f ON f.id=m.folder_id AND f.account_id=m.account_id \
                     WHERE m.account_id=?1 AND f.kind='inbox' AND m.unread=1 AND m.date>?2 \
                     ORDER BY m.date DESC, m.id DESC LIMIT ?3 \
                 ) ORDER BY date ASC, id ASC",
            )
            .map_err(sql_err)?;
        let candidates = stmt
            .query_map(
                params![account_id.as_str(), after, limit.max(1) as i64],
                |row| {
                    // T-101: the same door the reply Subject went through.
                    // `messages.sender_name`/`subject` are *supposed* to hold
                    // display text, but rows that predate the decoding sync
                    // path still hold `=?UTF-8?B?...?=`; the list decodes
                    // again when it paints, the desktop notification cannot.
                    // Decoding is idempotent, so plain text passes through.
                    let sender: String = row.get(1)?;
                    let subject: String = row.get(2)?;
                    Ok(NotificationCandidate {
                        thread_id: ThreadId(row.get(0)?),
                        sender: decode_encoded_words(&sender),
                        subject: decode_encoded_words(&subject),
                        date: row.get(3)?,
                    })
                },
            )
            .map_err(sql_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sql_err)?;
        Ok(candidates)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Core;
    use rusqlite::params;

    fn seed(core: &Core, subject: &str, sender_name: &str) {
        let conn = core.db.conn();
        conn.execute(
            "INSERT INTO accounts (id, name, email, provider, status, download_policy, created_at, updated_at) \
             VALUES ('a', 'a', 'me@example.com', 'generic', 'synced', 'recent', 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO folders (id, account_id, name, kind) VALUES ('a:inbox', 'a', 'Inbox', 'inbox')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO threads (id, account_id, folder_id, subject, snippet, date, unread, starred) \
             VALUES ('a:t', 'a', 'a:inbox', ?1, '', 100, 1, 0)",
            params![subject],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (id, account_id, thread_id, folder_id, date, sender_name, \
             sender_email, recipients, subject, unread, starred, has_attachment) \
             VALUES ('a:m', 'a', 'a:t', 'a:inbox', 100, ?2, 'her@example.com', '', ?1, 1, 0, 0)",
            params![subject, sender_name],
        )
        .unwrap();
    }

    /// T-101: the desktop notification is the one place a raw `=?UTF-8?B?...?=`
    /// row cannot be rescued by `display_subject` at paint time -- the string
    /// leaves the process. Rows that predate the decoding sync path still hold
    /// encoded words, so this query decodes on the way out.
    #[test]
    fn a_cyrillic_notification_says_words_not_encoded_ones() {
        let core = Core::memory().unwrap();
        seed(
            &core,
            "=?UTF-8?B?0J/RgNC40LLQtdGC?=",
            "=?UTF-8?B?0JDQvdC90LA=?=",
        );
        let got = core
            .notification_candidates(&AccountId("a".into()), 0, 10)
            .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].subject, "Привет");
        assert_eq!(got[0].sender, "Анна");
    }

    fn seed_burst(core: &Core, count: i64) {
        let conn = core.db.conn();
        conn.execute(
            "INSERT INTO accounts (id, name, email, provider, status, download_policy, created_at, updated_at) \
             VALUES ('a', 'a', 'me@example.com', 'generic', 'synced', 'recent', 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO folders (id, account_id, name, kind) VALUES ('a:inbox', 'a', 'Inbox', 'inbox')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO threads (id, account_id, folder_id, subject, snippet, date, unread, starred) \
             VALUES ('a:t', 'a', 'a:inbox', 'burst', '', ?1, 1, 0)",
            params![count],
        )
        .unwrap();
        for date in 1..=count {
            conn.execute(
                "INSERT INTO messages (id, account_id, thread_id, folder_id, date, sender_name, \
                 sender_email, recipients, subject, unread, starred, has_attachment) \
                 VALUES (?1, 'a', 'a:t', 'a:inbox', ?2, 'Ann', 'her@example.com', '', 'Burst', 1, 0, 0)",
                params![format!("a:m{date:03}"), date],
            )
            .unwrap();
        }
    }

    /// A sync tick that lands more unread mail than the notification limit
    /// must notify about the *newest* mail, not the oldest -- the limit
    /// cuts the least-important (oldest) backlog, not the letter that just
    /// arrived. (core-domain-04, Core half: the app half -- advancing the
    /// watermark from the dates actually notified rather than the
    /// inbox-wide `notification_watermark()` -- lives in
    /// `crates/app/src/shell.rs::notify_new_mail` and is out of this
    /// crate's reach to test directly.)
    ///
    /// T-159 settled the other half of this: a scalar date watermark
    /// cannot, on its own, ALSO guarantee that a burst bigger than the
    /// limit eventually notifies every message, because any watermark
    /// derived from the kept (newer) batch sits strictly above the dropped
    /// (older) one -- verified at 25 unread with limit 20, where dates
    /// 1..=5 were never notified. The answer is not to page the burst but
    /// to stop pretending it is a list of letters: past the limit the
    /// shell posts one summary for the whole tick
    /// (`App::new_mail_announcement`) and moves the watermark to the inbox
    /// maximum, because the summary covers every candidate. That decision
    /// needs the count *before* the limit cuts anything, which is what
    /// [`Core::notification_candidate_count`] is for.
    #[test]
    fn a_burst_larger_than_the_notification_limit_notifies_the_newest_mail_first() {
        let core = Core::memory().unwrap();
        seed_burst(&core, 25);
        let acc = AccountId("a".into());

        let batch = core.notification_candidates(&acc, 0, 20).unwrap();

        assert_eq!(batch.len(), 20, "the limit must still cap the batch size");
        assert_eq!(
            batch.iter().map(|c| c.date).max(),
            Some(25),
            "the newest unread message must be in the batch, not cut by the limit"
        );
        assert_eq!(
            batch.iter().map(|c| c.date).min(),
            Some(6),
            "the limit must cut the oldest backlog (dates 1..=5), keeping the newest 20"
        );
        assert!(
            batch.windows(2).all(|w| w[0].date < w[1].date),
            "candidates are still handed to the caller oldest-first for display"
        );
    }

    /// Core-only pin for the same contract, at the shape the surrounding
    /// audit used to exercise the app/shell watermark loop with: once the
    /// account is fully caught up (`notification_watermark` at the true
    /// inbox max), asking again with that watermark as `after` must not
    /// return anything -- and a first call for a fresh backlog must still
    /// respect the limit.
    #[test]
    fn watermark_reaches_the_inbox_max_and_a_caught_up_account_gets_no_more_candidates() {
        let core = Core::memory().unwrap();
        seed_burst(&core, 50);
        let acc = AccountId("a".into());

        assert_eq!(core.notification_watermark(&acc).unwrap(), 50);
        assert_eq!(core.notification_candidates(&acc, 0, 20).unwrap().len(), 20);
        assert!(core
            .notification_candidates(&acc, 50, 20)
            .unwrap()
            .is_empty());
    }

    /// T-159: the count and the candidate list must answer the same
    /// question, or the shell decides "burst or not" from one WHERE and
    /// then posts from another. Below the limit they agree exactly; above
    /// it the count keeps counting while the list stops at the limit,
    /// which is the whole reason the count exists.
    #[test]
    fn the_candidate_count_agrees_with_the_candidates_it_counts() {
        let core = Core::memory().unwrap();
        seed_burst(&core, 25);
        let acc = AccountId("a".into());

        assert_eq!(core.notification_candidate_count(&acc, 20).unwrap(), 5);
        assert_eq!(
            core.notification_candidates(&acc, 20, 20).unwrap().len(),
            5,
            "under the limit the count is the number of rows actually returned"
        );
        assert_eq!(
            core.notification_candidate_count(&acc, 0).unwrap(),
            25,
            "the count is taken before the limit, so a burst is visible as one"
        );
        assert_eq!(
            core.notification_candidates(&acc, 0, 20).unwrap().len(),
            20,
            "while the list itself is still capped"
        );
        assert_eq!(
            core.notification_candidate_count(&acc, 25).unwrap(),
            0,
            "a caught-up account counts nothing, the same way it returns nothing"
        );
    }

    /// Read mail is not a notification candidate, and the count has to
    /// agree with that -- otherwise a mailbox the reader has just been
    /// through would announce itself as a burst of nothing.
    #[test]
    fn read_mail_is_not_counted_as_a_notification_candidate() {
        let core = Core::memory().unwrap();
        seed_burst(&core, 4);
        let acc = AccountId("a".into());
        core.db
            .conn()
            .execute("UPDATE messages SET unread=0 WHERE date<=2", [])
            .unwrap();
        assert_eq!(core.notification_candidate_count(&acc, 0).unwrap(), 2);
        assert_eq!(core.notification_candidates(&acc, 0, 20).unwrap().len(), 2);
    }

    /// Decoding is idempotent: plain headers pass through untouched.
    #[test]
    fn a_plain_notification_is_unchanged() {
        let core = Core::memory().unwrap();
        seed(&core, "Project update", "Ann");
        let got = core
            .notification_candidates(&AccountId("a".into()), 0, 10)
            .unwrap();
        assert_eq!(got[0].subject, "Project update");
        assert_eq!(got[0].sender, "Ann");
    }
}
