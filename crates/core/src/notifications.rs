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

    pub fn notification_candidates(
        &self,
        account_id: &AccountId,
        after: i64,
        limit: usize,
    ) -> Result<Vec<NotificationCandidate>, CoreError> {
        self.require_account(account_id.as_str())?;
        let mut stmt = self
            .db
            .conn()
            .prepare(
                "SELECT m.thread_id, \
                        CASE WHEN m.sender_name='' THEN m.sender_email ELSE m.sender_name END, \
                        CASE WHEN m.subject='' THEN '(No subject)' ELSE m.subject END, m.date \
                 FROM messages m \
                 JOIN folders f ON f.id=m.folder_id AND f.account_id=m.account_id \
                 WHERE m.account_id=?1 AND f.kind='inbox' AND m.unread=1 AND m.date>?2 \
                 ORDER BY m.date ASC, m.id ASC LIMIT ?3",
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
