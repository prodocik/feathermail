//! Privacy-safe desktop notification payloads (T-053).

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationPayload {
    pub id: String,
    pub title: String,
    pub subject: String,
    pub account_id: String,
    pub thread_id: String,
}

impl NotificationPayload {
    pub fn new(
        account_id: impl Into<String>,
        thread_id: impl Into<String>,
        sender: impl Into<String>,
        subject: impl Into<String>,
    ) -> Self {
        let account_id = account_id.into();
        let thread_id = thread_id.into();
        Self {
            id: format!("new-mail:{account_id}:{thread_id}"),
            title: sender.into(),
            subject: subject.into(),
            account_id,
            thread_id,
        }
    }
}

/// The notifications this process has handed to the desktop shell and is
/// therefore on the hook to take back.
///
/// `send_notification` gives a notification away: from that moment the
/// shell owns it, keeps it in the shade after the process that sent it is
/// gone, and — with Ubuntu's dock counting notifications
/// (`show-icons-notifications-counter`) — paints it into the badge on the
/// launcher icon. Nothing expires on its own, and nothing connects a
/// notification to the mail it is about: reading the thread leaves the
/// entry and the badge exactly where they were. So the shell has to say
/// when a notification stopped being true — when its thread is read, when
/// its folder is marked read, and when the application quits.
///
/// Each entry keeps the account and thread it came from rather than only
/// the id, so "withdraw what this read covers" is a lookup and not a
/// re-parse of the id string.
///
/// The registry lives here rather than in the GTK shell so its one
/// invariant — every id sent is withdrawn, and withdrawn once — is
/// testable without a display.
#[derive(Debug, Default)]
pub struct PostedNotifications {
    /// Keyed by notification id, which is what the shell answers to.
    /// Ordered so the withdraw pass is deterministic, tests included.
    sent: std::collections::BTreeMap<String, PostedOrigin>,
}

/// What one still-standing notification is about.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PostedOrigin {
    account_id: String,
    thread_id: String,
}

impl PostedNotifications {
    /// Records one notification as sent.
    ///
    /// The same thread notified twice collapses to a single entry, which
    /// mirrors what the shell itself does: `send_notification` with an id
    /// already in use replaces that notification rather than adding a
    /// second one, so there is never a second copy to withdraw.
    pub fn record(&mut self, payload: &NotificationPayload) {
        self.sent.insert(
            payload.id.clone(),
            PostedOrigin {
                account_id: payload.account_id.clone(),
                thread_id: payload.thread_id.clone(),
            },
        );
    }

    /// Takes every id out, leaving the registry empty.
    ///
    /// Emptying is the point, not a side effect: both doors out of the
    /// process (the window closing, the application shutting down) run the
    /// withdraw pass, and the second one must find nothing left rather
    /// than write to a bus connection that is already going away.
    pub fn take_all(&mut self) -> Vec<String> {
        std::mem::take(&mut self.sent).into_keys().collect()
    }

    /// Takes the ids announcing any of `thread_ids` — what one read (or a
    /// batch of them) makes untrue.
    pub fn take_for_threads(&mut self, thread_ids: &[String]) -> Vec<String> {
        self.take_matching(|origin| thread_ids.contains(&origin.thread_id))
    }

    /// Takes the ids belonging to one account — "mark this folder read"
    /// on a single mailbox.
    pub fn take_for_account(&mut self, account_id: &str) -> Vec<String> {
        self.take_matching(|origin| origin.account_id == account_id)
    }

    /// Whether anything is owed to the shell right now.
    pub fn is_empty(&self) -> bool {
        self.sent.is_empty()
    }

    fn take_matching(&mut self, keep_out: impl Fn(&PostedOrigin) -> bool) -> Vec<String> {
        let taken: Vec<String> = self
            .sent
            .iter()
            .filter(|(_, origin)| keep_out(origin))
            .map(|(id, _)| id.clone())
            .collect();
        for id in &taken {
            self.sent.remove(id);
        }
        taken
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn posted(account: &str, thread: &str) -> NotificationPayload {
        NotificationPayload::new(account, thread, "Ada", "Invoice")
    }

    #[test]
    fn the_same_thread_notified_twice_is_withdrawn_once() {
        let mut registry = PostedNotifications::default();
        let payload = posted("a", "t");
        registry.record(&payload);
        registry.record(&payload);
        assert_eq!(registry.take_all(), vec![payload.id]);
    }

    #[test]
    fn taking_the_ids_leaves_nothing_for_a_second_pass() {
        let mut registry = PostedNotifications::default();
        registry.record(&posted("a", "t1"));
        registry.record(&posted("a", "t2"));
        assert_eq!(registry.take_all().len(), 2);
        assert!(registry.is_empty());
        assert!(
            registry.take_all().is_empty(),
            "window close already withdrew these; application shutdown must not talk to the bus again"
        );
    }

    #[test]
    fn a_registry_that_was_never_used_owes_nothing() {
        let mut registry = PostedNotifications::default();
        assert!(registry.is_empty());
        assert!(registry.take_all().is_empty());
    }

    #[test]
    fn reading_a_thread_takes_only_that_threads_notification() {
        let mut registry = PostedNotifications::default();
        let read = posted("a", "t1");
        let untouched = posted("a", "t2");
        registry.record(&read);
        registry.record(&untouched);
        assert_eq!(
            registry.take_for_threads(&["t1".to_string()]),
            vec![read.id]
        );
        assert_eq!(
            registry.take_all(),
            vec![untouched.id],
            "a thread still unread must keep announcing itself"
        );
    }

    /// The same thread id in two mailboxes is two different threads, and
    /// the account is what tells them apart.
    #[test]
    fn marking_one_account_read_leaves_the_other_accounts_notifications() {
        let mut registry = PostedNotifications::default();
        let mine = posted("work", "t1");
        let other = posted("home", "t1");
        registry.record(&mine);
        registry.record(&other);
        assert_eq!(registry.take_for_account("work"), vec![mine.id]);
        assert_eq!(registry.take_all(), vec![other.id]);
    }

    #[test]
    fn a_read_that_covers_nothing_posted_withdraws_nothing() {
        let mut registry = PostedNotifications::default();
        let kept = posted("a", "t1");
        registry.record(&kept);
        assert!(registry
            .take_for_threads(&["never-notified".to_string()])
            .is_empty());
        assert!(registry.take_for_account("someone-else").is_empty());
        assert_eq!(registry.take_all(), vec![kept.id]);
    }

    #[test]
    fn payload_has_sender_and_subject_but_no_body_field() {
        let payload = NotificationPayload::new("a", "t", "Ada", "Invoice");
        assert_eq!(payload.title, "Ada");
        assert_eq!(payload.subject, "Invoice");
        let debug = format!("{payload:?}");
        assert!(!debug.contains("message body"));
        assert!(!debug.contains("body:"));
    }
}
