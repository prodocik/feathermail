//! In-memory mail store matching `ui-preview/` fixtures. Replaced by Core+SQLite at T-007.

use crate::mailbox::{display_name_from_email, unique_account_id, AddAccountError};
use crate::model::{
    empty_copy, in_folder, matches_query, older_than_cursor, stamp_headers, Account, AccountId,
    AccountStatus, Address, CreateFolderError, Density, EmptyCopy, Folder, FolderId, FolderKind,
    Importance, ListRow, Placement, Theme, Thread, ThreadCursor, ThreadFilter, ThreadId,
    ThreadPage, FIXTURE_NOW,
};

pub const ACCOUNT_JOHN: &str = "john";
pub const ACCOUNT_JANE: &str = "jane";

#[derive(Clone, Debug)]
pub struct UndoSnapshot {
    pub label: String,
    threads: Vec<Thread>,
    selected: Option<String>,
}

#[derive(Clone, Debug)]
pub struct FakeMailStore {
    accounts: Vec<Account>,
    current: AccountId,
    default: AccountId,
    /// False until the first local metadata page exists (T-014 skeleton).
    cache: bool,
    pub now: i64,
    pub theme: Theme,
    pub density: Density,
    threads: Vec<Thread>,
    custom_folders: Vec<Folder>,
    undo: Option<UndoSnapshot>,
}

impl Default for FakeMailStore {
    fn default() -> Self {
        Self::seeded()
    }
}

impl FakeMailStore {
    pub fn seeded() -> Self {
        Self::with_filler(460)
    }

    /// Synthetic mailbox for T-013 (10k) and later T-060 benches.
    pub fn volume(n: usize) -> Self {
        Self::with_filler(n)
    }

    fn with_filler(filler: usize) -> Self {
        let mut threads = named_threads();
        if let Some(t) = threads
            .iter_mut()
            .find(|t| t.id.as_str() == "oliver-project")
        {
            t.message_count = 5;
            t.importance = Importance::High;
        }
        threads.extend(extra_threads());
        threads.extend(filler_threads(filler));
        threads.extend(special_threads());
        threads.extend(jane_threads());
        pad_unread(&mut threads);
        let mut folders = john_folders();
        folders.extend(jane_folders());
        Self {
            accounts: vec![john_account(), jane_account()],
            current: AccountId(ACCOUNT_JOHN.into()),
            default: AccountId(ACCOUNT_JOHN.into()),
            cache: true,
            now: FIXTURE_NOW,
            theme: Theme::Light,
            density: Density::Comfortable,
            threads,
            custom_folders: folders,
            undo: None,
        }
    }

    /// No local mailbox data. List shows skeleton, not empty copy (T-014 / D45).
    pub fn uncached() -> Self {
        Self {
            accounts: vec![john_account()],
            current: AccountId(ACCOUNT_JOHN.into()),
            default: AccountId(ACCOUNT_JOHN.into()),
            cache: false,
            now: FIXTURE_NOW,
            theme: Theme::Light,
            density: Density::Comfortable,
            threads: Vec::new(),
            custom_folders: Vec::new(),
            undo: None,
        }
    }

    pub fn cache_empty(&self) -> bool {
        !self.cache
    }

    /// First metadata (or the next page) lands locally. Rows append; cache becomes ready.
    pub fn arrive(&mut self, threads: impl IntoIterator<Item = Thread>) {
        self.cache = true;
        self.threads.extend(threads);
    }

    pub fn accounts(&self) -> &[Account] {
        &self.accounts
    }

    pub fn account(&self) -> &Account {
        self.accounts
            .iter()
            .find(|a| a.id == self.current)
            .unwrap_or(&self.accounts[0])
    }

    pub fn default_account(&self) -> &Account {
        self.accounts
            .iter()
            .find(|a| a.id == self.default)
            .unwrap_or(&self.accounts[0])
    }

    pub fn current_account_id(&self) -> &AccountId {
        &self.current
    }

    /// D21: filter subsequent queries by `account_id`. No restart.
    pub fn switch_account(&mut self, id: &str) -> bool {
        if self.accounts.iter().any(|a| a.id.as_str() == id) {
            self.current = AccountId(id.into());
            true
        } else {
            false
        }
    }

    /// T-017: add a mailbox locally. No IMAP. Secret stays in the keyring.
    pub fn add_account(&mut self, email: &str) -> Result<AccountId, AddAccountError> {
        let email = email.trim();
        if !email.contains('@')
            || email.starts_with('@')
            || email.ends_with('@')
            || email.matches('@').count() != 1
        {
            return Err(AddAccountError::Email);
        }
        if self
            .accounts
            .iter()
            .any(|a| a.email.eq_ignore_ascii_case(email))
        {
            return Err(AddAccountError::Duplicate);
        }
        let id = unique_account_id(email, self.accounts.iter().map(|a| a.id.as_str())).0;
        let account = Account {
            id: AccountId(id.clone()),
            name: display_name_from_email(email),
            email: email.to_string(),
            status: AccountStatus::Syncing,
        };
        self.accounts.push(account);
        self.current = AccountId(id.clone());
        Ok(AccountId(id))
    }

    /// T-021: drop an account and everything scoped to it from the fixture
    /// store (mail, custom folders). Mirrors `Core::remove_account`'s
    /// per-account sweep, just against the in-memory fixture instead of
    /// SQLite. If the removed account was current/default, falls back to
    /// whatever account is left; the caller is responsible for noticing an
    /// empty account list and routing to the Welcome screen.
    pub fn remove_account(&mut self, id: &str) -> bool {
        let Some(pos) = self.accounts.iter().position(|a| a.id.as_str() == id) else {
            return false;
        };
        self.accounts.remove(pos);
        let removed = AccountId(id.to_string());
        self.threads.retain(|t| t.account_id != removed);
        self.custom_folders
            .retain(|f| f.account_id.as_ref() != Some(&removed));
        if self.current == removed {
            self.current = self
                .accounts
                .first()
                .map(|a| a.id.clone())
                .unwrap_or_else(|| removed.clone());
        }
        if self.default == removed {
            self.default = self
                .accounts
                .first()
                .map(|a| a.id.clone())
                .unwrap_or(removed);
        }
        true
    }

    /// T-021: rename the display name shown for an account (Settings ->
    /// Accounts -> Edit). Email/identifier is never editable here, matching
    /// `Core::update_account`'s rule that the identifier is immutable.
    pub fn rename_account(&mut self, id: &str, name: &str) -> bool {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return false;
        }
        match self.accounts.iter_mut().find(|a| a.id.as_str() == id) {
            Some(account) => {
                account.name = trimmed.to_string();
                true
            }
            None => false,
        }
    }

    pub fn set_account_status(&mut self, id: &str, status: AccountStatus) -> bool {
        if let Some(account) = self.accounts.iter_mut().find(|a| a.id.as_str() == id) {
            account.status = status;
            true
        } else {
            false
        }
    }

    pub fn has_folder(&self, id: &str) -> bool {
        Self::system_folders().iter().any(|f| f.id.as_str() == id)
            || self.custom_folders().any(|f| f.id.as_str() == id)
    }

    pub fn system_folders() -> Vec<Folder> {
        [
            ("inbox", "Inbox", FolderKind::Inbox),
            ("starred", "Starred", FolderKind::Starred),
            ("snoozed", "Snoozed", FolderKind::Snoozed),
            ("sent", "Sent", FolderKind::Sent),
            ("drafts", "Drafts", FolderKind::Drafts),
            ("archive", "Archive", FolderKind::Archive),
            ("spam", "Spam", FolderKind::Spam),
            ("trash", "Trash", FolderKind::Trash),
        ]
        .into_iter()
        .map(|(id, label, kind)| Folder {
            id: FolderId(id.into()),
            label: label.into(),
            kind,
            color: None,
            account_id: None,
            create_failed: false,
        })
        .collect()
    }

    pub fn custom_folders(&self) -> impl Iterator<Item = &Folder> {
        let current = &self.current;
        self.custom_folders
            .iter()
            .filter(move |f| f.account_id.as_ref() == Some(current))
    }

    pub fn folder_label(&self, id: &str) -> String {
        Self::system_folders()
            .into_iter()
            .find(|f| f.id.as_str() == id)
            .map(|f| f.label)
            .or_else(|| {
                self.custom_folders
                    .iter()
                    .find(|f| f.id.as_str() == id)
                    .map(|f| f.label.clone())
            })
            .unwrap_or_else(|| "Inbox".into())
    }

    pub fn unread_count(&self, folder: &str) -> u32 {
        self.threads
            .iter()
            .filter(|t| t.account_id == self.current && t.unread() && in_folder(folder, t))
            .count() as u32
    }

    pub fn list_threads(&self, folder: &str, filter: ThreadFilter, query: &str) -> Vec<&Thread> {
        let mut list: Vec<&Thread> = self
            .threads
            .iter()
            .filter(|t| self.listed(t, folder, filter, query))
            .collect();
        list.sort_by(|a, b| {
            b.date
                .cmp(&a.date)
                .then_with(|| b.id.as_str().cmp(a.id.as_str()))
        });
        list
    }

    pub fn listed(&self, t: &Thread, folder: &str, filter: ThreadFilter, query: &str) -> bool {
        if t.account_id != self.current {
            return false;
        }
        if !in_folder(folder, t) {
            return false;
        }
        match filter {
            ThreadFilter::All => {}
            ThreadFilter::Unread => {
                if !t.unread() {
                    return false;
                }
            }
            ThreadFilter::Starred => {
                if !t.starred {
                    return false;
                }
            }
            ThreadFilter::Attachments => {
                if !t.has_attachment {
                    return false;
                }
            }
        }
        let q = query.trim();
        q.is_empty() || matches_query(t, q)
    }

    /// Cursor page of already-sorted threads. Does not clone the rest of the mailbox.
    pub fn list_page(
        &self,
        folder: &str,
        filter: ThreadFilter,
        query: &str,
        after: Option<&ThreadCursor>,
        before: Option<&ThreadCursor>,
        limit: usize,
    ) -> ThreadPage {
        let mut idxs: Vec<usize> = self
            .threads
            .iter()
            .enumerate()
            .filter(|(_, t)| self.listed(t, folder, filter, query))
            .map(|(i, _)| i)
            .collect();
        idxs.sort_by(|&a, &b| {
            let ta = &self.threads[a];
            let tb = &self.threads[b];
            tb.date
                .cmp(&ta.date)
                .then_with(|| tb.id.as_str().cmp(ta.id.as_str()))
        });
        let total = idxs.len();
        let (start, end) = if let Some(cur) = after {
            let mut i = 0;
            while i < idxs.len() {
                let t = &self.threads[idxs[i]];
                if t.date == cur.date && t.id.as_str() == cur.id.as_str() {
                    i += 1;
                    break;
                }
                if older_than_cursor(t, cur) {
                    break;
                }
                i += 1;
            }
            let start = i;
            (start, start.saturating_add(limit).min(total))
        } else if let Some(cur) = before {
            let end = idxs
                .iter()
                .position(|&i| {
                    let t = &self.threads[i];
                    t.date == cur.date && t.id.as_str() == cur.id.as_str()
                })
                .unwrap_or_else(|| {
                    idxs.iter()
                        .position(|&i| older_than_cursor(&self.threads[i], cur))
                        .unwrap_or(total)
                });
            (end.saturating_sub(limit), end)
        } else {
            (0, limit.min(total))
        };
        let threads: Vec<Thread> = idxs[start..end]
            .iter()
            .map(|&i| self.threads[i].clone())
            .collect();
        let next = if end < total {
            threads.last().map(ThreadCursor::of)
        } else {
            None
        };
        let prev = if start > 0 {
            threads.first().map(ThreadCursor::of)
        } else {
            None
        };
        ThreadPage {
            threads,
            next,
            prev,
            total,
        }
    }

    pub fn list_rows(&self, folder: &str, filter: ThreadFilter, query: &str) -> Vec<ListRow> {
        let page = self.list_page(folder, filter, query, None, None, usize::MAX);
        stamp_headers(page.threads.iter(), self.now, None).0
    }

    pub fn get(&self, id: &str) -> Option<&Thread> {
        self.threads.iter().find(|t| t.id.as_str() == id)
    }

    pub fn empty(&self, folder: &str, query: &str) -> EmptyCopy {
        empty_copy(folder, !query.trim().is_empty())
    }

    pub fn take_undo(&mut self) -> Option<UndoSnapshot> {
        self.undo.take()
    }

    pub fn peek_undo(&self) -> Option<&UndoSnapshot> {
        self.undo.as_ref()
    }

    fn snapshot(&self, label: &str, selected: Option<&str>) -> UndoSnapshot {
        UndoSnapshot {
            label: label.into(),
            threads: self.threads.clone(),
            selected: selected.map(str::to_string),
        }
    }

    fn apply_ids(&mut self, ids: &[String], mut f: impl FnMut(&mut Thread)) {
        for t in &mut self.threads {
            if ids.iter().any(|id| t.id.as_str() == id) {
                f(t);
            }
        }
    }

    pub fn archive(&mut self, ids: &[String], selected: Option<&str>) {
        self.undo = Some(self.snapshot("Conversation archived", selected));
        self.apply_ids(ids, Thread::archive);
    }

    pub fn trash(&mut self, ids: &[String], selected: Option<&str>) {
        self.undo = Some(self.snapshot("Conversation moved to Trash", selected));
        self.apply_ids(ids, Thread::trash);
    }

    pub fn mark_read(&mut self, id: &str, read: bool) {
        self.apply_ids(&[id.to_string()], |t| t.set_unread(!read));
    }

    pub fn mark_read_many(&mut self, ids: &[String], read: bool) {
        self.apply_ids(ids, |t| t.set_unread(!read));
    }

    pub fn toggle_star(&mut self, id: &str) {
        if let Some(t) = self.threads.iter_mut().find(|t| t.id.as_str() == id) {
            t.starred = !t.starred;
        }
    }

    pub fn snooze(&mut self, ids: &[String], selected: Option<&str>) {
        self.undo = Some(self.snapshot("Snoozed until tomorrow", selected));
        let until = self.now + 86_400 - (self.now.rem_euclid(86_400)) + 9 * 3600;
        self.apply_ids(ids, |t| t.snooze(until));
    }

    pub fn undo(&mut self) -> Option<String> {
        let snap = self.undo.take()?;
        self.threads = snap.threads;
        snap.selected
    }

    pub fn add_custom_folder(&mut self, name: &str) {
        let _ = self.create_folder(name);
    }

    pub fn create_folder(&mut self, name: &str) -> Result<FolderId, CreateFolderError> {
        let label = name.trim();
        if let Some(err) = crate::folder_label_error(label) {
            return Err(err);
        }
        let id = slug(label);
        let clash = |f: &Folder| f.id.as_str() == id || f.label.eq_ignore_ascii_case(label);
        if Self::system_folders().iter().any(clash) {
            return Err(CreateFolderError::SystemName);
        }
        if self.custom_folders().any(clash) {
            return Err(CreateFolderError::Duplicate);
        }
        let color = FOLDER_PALETTE[self.custom_folders.len() % FOLDER_PALETTE.len()];
        self.custom_folders.push(Folder {
            id: FolderId(id.clone()),
            label: label.into(),
            kind: FolderKind::Custom,
            color: Some(color),
            account_id: Some(self.current.clone()),
            create_failed: false,
        });
        Ok(FolderId(id))
    }
}

const FOLDER_PALETTE: [&str; 5] = ["#47CC50", "#9451F4", "#FB954A", "#4181F3", "#2DD2E0"];

fn slug(name: &str) -> String {
    name.trim()
        .to_ascii_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("-")
}

fn at(hour: i64, min: i64, day_offset: i64) -> i64 {
    let day = FIXTURE_NOW.div_euclid(86_400) + day_offset;
    day * 86_400 + hour * 3600 + min * 60
}

fn addr(name: &str, email: &str) -> Address {
    Address {
        name: name.into(),
        email: email.into(),
    }
}

#[allow(clippy::too_many_arguments)]
fn thread(
    id: &str,
    folder: &str,
    from: Address,
    to: &str,
    subject: &str,
    preview: &str,
    date: i64,
    unread: bool,
    starred: bool,
    labels: &[&str],
    attach: bool,
    body: &str,
) -> Thread {
    Thread {
        id: ThreadId(id.into()),
        account_id: AccountId(ACCOUNT_JOHN.into()),
        folder: FolderId(folder.into()),
        from,
        to: to.into(),
        subject: subject.into(),
        preview: preview.into(),
        date,
        placement: Placement::Active { unread },
        starred,
        labels: labels.iter().map(|s| (*s).to_string()).collect(),
        has_attachment: attach,
        importance: Importance::Normal,
        message_count: 1,
        body_html: body.into(),
        // Fixture threads have no backing `messages` row (FakeMailStore
        // never touches SQLite) -- there is nothing for T-024's body cache
        // to fetch here, so this is honestly `None`, not a made-up id.
        message_id: None,
    }
}

fn named_threads() -> Vec<Thread> {
    let notion = "
    <p>Hi John,</p>
    <p>We're updating our Terms of Service to better explain what Notion offers and how we protect your data.</p>
    <p>These updates will take effect on May 28, 2024. You can read the updated Terms of Service in full.</p>
    <p>If you have any questions, feel free to reach out to us.</p>
    <p>Thanks,<br>The Notion Team</p>
  ";
    vec![
        thread(
            "notion-tos",
            "inbox",
            addr("Notion Team", "team@notion.so"),
            "me",
            "Updates to our Terms of Service",
            "We're updating our Terms of Service to…",
            at(10, 45, 0),
            true,
            false,
            &["Inbox"],
            false,
            notion,
        ),
        thread(
            "linkedin-weekly",
            "inbox",
            addr("LinkedIn", "updates@linkedin.com"),
            "me",
            "Your weekly LinkedIn update",
            "See what's new in your network…",
            at(9, 30, 0),
            true,
            false,
            &["Inbox"],
            false,
            "<p>See what's new in your network this week — 14 new posts from people you follow.</p>",
        ),
        thread(
            "oliver-project",
            "inbox",
            addr("Oliver Smith", "oliver@example.com"),
            "me",
            "Re: Project update",
            "Sounds good, let's catch up tomorrow.",
            at(8, 15, 0),
            false,
            true,
            &["Inbox", "Work"],
            false,
            "<p>Sounds good, let's catch up tomorrow.</p><p>I'll bring the latest numbers.</p>",
        ),
        thread(
            "github-verify",
            "inbox",
            addr("GitHub", "noreply@github.com"),
            "me",
            "[GitHub] Please verify your email",
            "Verify your email address to access…",
            at(18, 2, -1),
            true,
            false,
            &["Inbox"],
            false,
            "<p>Verify your email address to access your GitHub account.</p>",
        ),
        thread(
            "stripe-payout",
            "inbox",
            addr("Stripe", "payouts@stripe.com"),
            "me",
            "Your payout has been sent",
            "Your payout of $1,250.00 has been…",
            at(14, 40, -1),
            false,
            false,
            &["Inbox", "Receipts"],
            true,
            "<p>Your payout of $1,250.00 has been sent to your bank account.</p>",
        ),
        thread(
            "figma-comment",
            "inbox",
            addr("Figma", "noreply@figma.com"),
            "me",
            "New comment on Landing Page",
            "Someone left a comment on Landing…",
            at(16, 5, -2),
            false,
            false,
            &["Inbox", "Projects"],
            false,
            "<p>Someone left a comment on <strong>Landing Page</strong>.</p><p>“Can we try a cooler paper white here?”</p>",
        ),
    ]
}

fn extra_threads() -> Vec<Thread> {
    let rows: [(&str, &str, &str, &str, bool, &str); 35] = [
        (
            "Ada Lovelace",
            "ada@example.com",
            "Notes from the lab",
            "The analytical engine notes are attached.",
            true,
            "Work",
        ),
        (
            "Linear",
            "noreply@linear.app",
            "3 issues assigned to you",
            "FM-12 keyboard path still open.",
            true,
            "Projects",
        ),
        (
            "Maya Chen",
            "maya@studio.co",
            "Re: invoice",
            "Paid — receipt attached.",
            false,
            "Receipts",
        ),
        (
            "Vercel",
            "tom.h@example.org",
            "Deployment ready",
            "feathermail-preview is live.",
            true,
            "Projects",
        ),
        (
            "Alex Rivera",
            "alex@example.com",
            "Lunch on Thursday?",
            "That cafe near the river still good?",
            false,
            "Personal",
        ),
        (
            "Dropbox",
            "no-reply@dropbox.com",
            "You edited Design.fig",
            "A copy is in your paper folder.",
            false,
            "Work",
        ),
        (
            "Samira Haddad",
            "samira@example.com",
            "Travel itinerary",
            "Flights land at 16:40 local.",
            false,
            "Travel",
        ),
        (
            "GitLab",
            "gitlab@gitlab.com",
            "Pipeline failed on main",
            "check-git-identity is red.",
            true,
            "Projects",
        ),
        (
            "Robin",
            "robin@example.com",
            "Photos from Lisbon",
            "The tiles came out well.",
            false,
            "Travel",
        ),
        (
            "Notion Team",
            "team@notion.so",
            "Weekly recap",
            "12 comments across 4 pages.",
            true,
            "Work",
        ),
        (
            "Bank",
            "alerts@bank.example",
            "Statement ready",
            "April statement is ready to download.",
            false,
            "Receipts",
        ),
        (
            "Priya Nair",
            "priya@example.com",
            "Draft intro",
            "How does this opener read?",
            false,
            "Work",
        ),
        (
            "Figma",
            "noreply@figma.com",
            "File shared with you",
            "Inbox shell — comments on.",
            false,
            "Projects",
        ),
        (
            "Kit",
            "kit@example.com",
            "Re: keys",
            "Left them with the front desk.",
            false,
            "Personal",
        ),
        (
            "Apple",
            "developer@apple.com",
            "Agreement updated",
            "Please review the new terms.",
            true,
            "Inbox",
        ),
        (
            "Zoom",
            "noreply@zoom.us",
            "Cloud recording",
            "The stand-up is ready.",
            false,
            "Work",
        ),
        (
            "Elena",
            "elena@example.com",
            "Birthday dinner",
            "Saturday 19:00, usual place.",
            false,
            "Personal",
        ),
        (
            "AWS",
            "no-reply@amazon.com",
            "Invoice available",
            "Your monthly invoice is ready.",
            false,
            "Receipts",
        ),
        (
            "Tomás",
            "tomas@example.com",
            "Hotel in Kyoto",
            "I booked the small ryokan.",
            false,
            "Travel",
        ),
        (
            "Calendly",
            "noreply@calendly.com",
            "New event scheduled",
            "Design review, tomorrow 11:00.",
            false,
            "Work",
        ),
        (
            "Jess Park",
            "jess@example.com",
            "Re: copy pass",
            "Quieter on the empty states.",
            false,
            "Projects",
        ),
        (
            "Fastmail",
            "support@fastmail.com",
            "Welcome to Fastmail",
            "Your mailbox is ready.",
            true,
            "Inbox",
        ),
        (
            "News",
            "editors@news.example",
            "Morning briefing",
            "Five things before the commute.",
            false,
            "Inbox",
        ),
        (
            "Omar",
            "omar@example.com",
            "Patch notes",
            "Virtual list no longer rebuilds.",
            false,
            "Projects",
        ),
        (
            "Expedia",
            "trips@expedia.com",
            "Your trip to Porto",
            "Boarding pass is attached.",
            false,
            "Travel",
        ),
        (
            "HR",
            "hr@example.com",
            "PTO approved",
            "May 29–31 is on the calendar.",
            false,
            "Work",
        ),
        (
            "Nico",
            "nico@example.com",
            "Guitar night",
            "Bring the spare strings.",
            false,
            "Personal",
        ),
        (
            "Cloudflare",
            "noreply@cloudflare.com",
            "Certificate renewed",
            "feathermail.app is covered.",
            false,
            "Projects",
        ),
        (
            "Ikea",
            "order@ikea.com",
            "Order shipped",
            "A desk lamp is on the way.",
            false,
            "Receipts",
        ),
        (
            "Leah",
            "leah@example.com",
            "Re: welcome copy",
            "Without the clutter still sings.",
            false,
            "Work",
        ),
        (
            "Spotify",
            "no-reply@spotify.com",
            "Your 2024 recap",
            "A quieter year. Still good.",
            false,
            "Personal",
        ),
        (
            "DHL",
            "shipment@dhl.com",
            "Out for delivery",
            "Arrive by 18:00.",
            false,
            "Travel",
        ),
        (
            "Mateo",
            "mateo@example.com",
            "Board notes",
            "I marked the P0 gate.",
            true,
            "Work",
        ),
        (
            "MDN",
            "noreply@developer.mozilla.org",
            "What's new in CSS",
            "color-mix is everywhere.",
            false,
            "Inbox",
        ),
        (
            "Ada Lovelace",
            "ada@example.com",
            "Follow-up",
            "Did the notes land?",
            false,
            "Work",
        ),
    ];
    rows.iter()
        .enumerate()
        .map(|(i, (name, email, subject, preview, unread, label))| {
            let day = if i < 2 { -3 } else { -14 - i as i64 };
            let hour = 9 + (i as i64 % 8);
            let labels: Vec<&str> = if *label == "Inbox" {
                vec!["Inbox"]
            } else {
                vec!["Inbox", label]
            };
            thread(
                &format!("named-{i}"),
                "inbox",
                addr(name, email),
                "me",
                subject,
                preview,
                at(hour, (i as i64 * 7) % 60, day),
                *unread,
                i == 2 || i == 11,
                &labels,
                i % 7 == 0,
                &format!("<p>{preview}</p>"),
            )
        })
        .collect()
}

fn filler_threads(n: usize) -> Vec<Thread> {
    const NAMES: [&str; 10] = [
        "Jordan Lee",
        "Casey Ng",
        "Riley Brooks",
        "Avery Cole",
        "Quinn Patel",
        "Morgan Díaz",
        "Jamie Fox",
        "Taylor Kim",
        "Drew Ali",
        "Sky Nakamura",
    ];
    const SUBJECTS: [&str; 10] = [
        "Quick check-in",
        "Following up",
        "Notes",
        "Can we move this?",
        "FYI",
        "Schedule",
        "Draft for review",
        "Thanks",
        "Ping",
        "Looping you in",
    ];
    (0..n)
        .map(|i| {
            let name = NAMES[i % NAMES.len()];
            let email = format!(
                "{}@mail.example",
                name.to_ascii_lowercase().replace(' ', ".")
            );
            thread(
                &format!("gen-{i}"),
                "inbox",
                addr(name, &email),
                "me",
                &format!("{} #{}", SUBJECTS[i % SUBJECTS.len()], i + 1),
                "This is fixture copy for the virtual list. It should never hitch the scroll.",
                at(8 + (i as i64 % 10), i as i64 % 60, -12 - (i as i64 / 8)),
                false,
                false,
                &["Inbox"],
                i % 11 == 0,
                "<p>Fixture body for scroll performance. Nothing to see here.</p>",
            )
        })
        .collect()
}

fn special_threads() -> Vec<Thread> {
    let mut drafts = vec![
        thread(
            "draft-1",
            "drafts",
            addr("John Doe", "john.doe@example.com"),
            "oliver@example.com",
            "Project update",
            "Here's where we landed on the paper inbox…",
            at(9, 5, 0),
            false,
            false,
            &["Drafts"],
            false,
            "<p>Here's where we landed on the paper inbox.</p>",
        ),
        thread(
            "draft-2",
            "drafts",
            addr("John Doe", "john.doe@example.com"),
            "maya@studio.co",
            "Invoice received",
            "Thanks — booked.",
            at(18, 40, -1),
            false,
            false,
            &["Drafts"],
            false,
            "<p>Thanks — booked.</p>",
        ),
        thread(
            "draft-3",
            "drafts",
            addr("John Doe", "john.doe@example.com"),
            "leah@example.com",
            "Welcome line",
            "Your email, without the clutter.",
            at(21, 12, -3),
            false,
            false,
            &["Drafts"],
            false,
            "<p>Your email, without the clutter.</p>",
        ),
        thread(
            "sent-1",
            "sent",
            addr("John Doe", "john.doe@example.com"),
            "oliver@example.com",
            "Re: Project update",
            "I'll send the numbers tonight.",
            at(7, 50, 0),
            false,
            false,
            &["Sent"],
            false,
            "<p>I'll send the numbers tonight.</p>",
        ),
    ];
    let mut snooze = thread(
        "snooze-1",
        "inbox",
        addr("Board", "board@example.com"),
        "me",
        "Decide on density default",
        "Comfortable stays unless we hear otherwise.",
        at(11, 0, -4),
        false,
        false,
        &["Snoozed"],
        false,
        "<p>Comfortable stays unless we hear otherwise.</p>",
    );
    snooze.placement = Placement::Snoozed {
        until: at(9, 0, 1),
        unread: false,
    };
    let mut archive = thread(
        "archive-1",
        "inbox",
        addr("Old list", "list@example.com"),
        "me",
        "You unsubscribed",
        "You will not hear from us again.",
        at(12, 0, -30),
        false,
        false,
        &["Archive"],
        false,
        "<p>You will not hear from us again.</p>",
    );
    archive.archive();
    drafts.push(snooze);
    drafts.push(archive);
    drafts
}

fn pad_unread(threads: &mut [Thread]) {
    let mut unread = threads
        .iter()
        .filter(|t| {
            t.unread()
                && t.account_id.as_str() == ACCOUNT_JOHN
                && t.folder.as_str() == "inbox"
                && matches!(t.placement, Placement::Active { .. })
        })
        .count();
    for t in threads.iter_mut() {
        if unread >= 12 {
            break;
        }
        if t.account_id.as_str() == ACCOUNT_JOHN
            && t.folder.as_str() == "inbox"
            && !t.unread()
            && matches!(t.placement, Placement::Active { .. })
            && t.id.as_str().starts_with("named-")
        {
            t.set_unread(true);
            unread += 1;
        }
    }
}

fn john_account() -> Account {
    Account {
        id: AccountId(ACCOUNT_JOHN.into()),
        name: "John Doe".into(),
        email: "john.doe@example.com".into(),
        status: AccountStatus::Synced,
    }
}

fn jane_account() -> Account {
    Account {
        id: AccountId(ACCOUNT_JANE.into()),
        name: "Jane Roe".into(),
        email: "jane.roe@example.com".into(),
        status: AccountStatus::Synced,
    }
}

fn owned(t: Thread, account: &str) -> Thread {
    let mut t = t;
    t.account_id = AccountId(account.into());
    t
}

fn jane_threads() -> Vec<Thread> {
    vec![
        owned(
            thread(
                "jane-maya",
                "inbox",
                addr("Maya Chen", "maya@studio.co"),
                "me",
                "Q3 design brief",
                "Attaching the quieter paper inbox notes.",
                at(10, 12, 0),
                true,
                false,
                &["Inbox"],
                false,
                "<p>Attaching the quieter paper inbox notes.</p>",
            ),
            ACCOUNT_JANE,
        ),
        owned(
            thread(
                "jane-legal",
                "inbox",
                addr("Acme Legal", "legal@acme.test"),
                "me",
                "NDA signed",
                "Countersigned copy is in Clients.",
                at(16, 40, -1),
                false,
                false,
                &["Inbox", "Clients"],
                true,
                "<p>Countersigned copy is in Clients.</p>",
            ),
            ACCOUNT_JANE,
        ),
        owned(
            thread(
                "jane-pager",
                "inbox",
                addr("Pager", "alerts@pager.test"),
                "me",
                "Latency blip resolved",
                "p95 is back under 120ms.",
                at(7, 5, 0),
                false,
                true,
                &["Inbox"],
                false,
                "<p>p95 is back under 120ms.</p>",
            ),
            ACCOUNT_JANE,
        ),
        owned(
            thread(
                "jane-sent",
                "sent",
                addr("Jane Roe", "jane.roe@example.com"),
                "maya@studio.co",
                "Re: Q3 design brief",
                "Ship the quieter empty states.",
                at(11, 2, 0),
                false,
                false,
                &["Sent"],
                false,
                "<p>Ship the quieter empty states.</p>",
            ),
            ACCOUNT_JANE,
        ),
        owned(
            thread(
                "jane-draft",
                "drafts",
                addr("Jane Roe", "jane.roe@example.com"),
                "legal@acme.test",
                "Follow-up on NDA",
                "Thanks — filed.",
                at(9, 20, 0),
                false,
                false,
                &["Drafts"],
                false,
                "<p>Thanks — filed.</p>",
            ),
            ACCOUNT_JANE,
        ),
    ]
}

fn john_folders() -> Vec<Folder> {
    custom_named(
        ACCOUNT_JOHN,
        [
            ("work", "Work", "#47CC50"),
            ("personal", "Personal", "#9451F4"),
            ("projects", "Projects", "#FB954A"),
            ("receipts", "Receipts", "#4181F3"),
            ("travel", "Travel", "#2DD2E0"),
        ],
    )
}

fn jane_folders() -> Vec<Folder> {
    custom_named(ACCOUNT_JANE, [("clients", "Clients", "#FB954A")])
}

fn custom_named<const N: usize>(
    account: &str,
    rows: [(&'static str, &'static str, &'static str); N],
) -> Vec<Folder> {
    rows.into_iter()
        .map(|(id, label, color)| Folder {
            id: FolderId(id.into()),
            label: label.into(),
            kind: FolderKind::Custom,
            color: Some(color),
            account_id: Some(AccountId(account.into())),
            create_failed: false,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::LIST_PAGE;

    #[test]
    fn inbox_unread_is_twelve() {
        let store = FakeMailStore::seeded();
        assert_eq!(store.unread_count("inbox"), 12);
    }

    #[test]
    fn inbox_opens_on_notion() {
        let store = FakeMailStore::seeded();
        let list = store.list_threads("inbox", ThreadFilter::All, "");
        assert_eq!(list[0].id.as_str(), "notion-tos");
        assert_eq!(list[0].from.name, "Notion Team");
    }

    #[test]
    fn spam_is_empty() {
        let store = FakeMailStore::seeded();
        assert!(store.list_threads("spam", ThreadFilter::All, "").is_empty());
    }

    #[test]
    fn archive_then_undo_restores_inbox() {
        let mut store = FakeMailStore::seeded();
        let before = store.list_threads("inbox", ThreadFilter::All, "").len();
        store.archive(&["notion-tos".into()], Some("notion-tos"));
        assert!(!store
            .list_threads("inbox", ThreadFilter::All, "")
            .iter()
            .any(|t| t.id.as_str() == "notion-tos"));
        let restored = store.undo();
        assert_eq!(restored.as_deref(), Some("notion-tos"));
        assert_eq!(
            store.list_threads("inbox", ThreadFilter::All, "").len(),
            before
        );
    }

    #[test]
    fn work_folder_contains_oliver() {
        let store = FakeMailStore::seeded();
        let work = store.list_threads("work", ThreadFilter::All, "");
        assert!(work.iter().any(|t| t.id.as_str() == "oliver-project"));
    }

    #[test]
    fn date_groups_start_with_today() {
        let store = FakeMailStore::seeded();
        let page = store.list_page("inbox", ThreadFilter::All, "", None, None, LIST_PAGE);
        let (rows, _) = stamp_headers(page.threads.iter(), store.now, None);
        assert!(matches!(rows.first(), Some(ListRow::Header(h)) if h == "Today"));
    }

    #[test]
    fn virtual_list_has_fixture_volume() {
        let store = FakeMailStore::seeded();
        assert!(store.list_threads("inbox", ThreadFilter::All, "").len() >= 400);
    }

    #[test]
    fn ten_k_page_does_not_clone_the_mailbox() {
        let store = FakeMailStore::volume(10_000);
        let page = store.list_page("inbox", ThreadFilter::All, "", None, None, LIST_PAGE);
        assert_eq!(page.threads.len(), LIST_PAGE);
        assert!(page.total >= 10_000);
        assert!(page.next.is_some());
        assert!(page.prev.is_none());
        let first = page.threads[0].id.clone();
        let page2 = store.list_page(
            "inbox",
            ThreadFilter::All,
            "",
            page.next.as_ref(),
            None,
            LIST_PAGE,
        );
        assert_eq!(page2.threads.len(), LIST_PAGE);
        assert_ne!(page2.threads[0].id, first);
        assert!(page.threads.iter().all(|t| t.id != page2.threads[0].id));
        let (r1, carry) = stamp_headers(page.threads.iter(), store.now, None);
        let (r2, _) = stamp_headers(page2.threads.iter(), store.now, carry);
        assert!(r1.len() <= LIST_PAGE + 4);
        assert!(r2.len() <= LIST_PAGE + 4);
        if let (Some(ListRow::Thread(a)), Some(ListRow::Thread(b))) = (r1.last(), r2.first()) {
            assert_ne!(a.id, b.id);
        }
    }

    #[test]
    fn midnight_clock_moves_the_today_header() {
        let mut store = FakeMailStore::seeded();
        store.now = 1_716_249_600;
        let page = store.list_page("inbox", ThreadFilter::All, "", None, None, LIST_PAGE);
        let (rows, _) = stamp_headers(page.threads.iter(), store.now, None);
        assert!(matches!(rows.first(), Some(ListRow::Header(h)) if h == "Yesterday"));
    }

    #[test]
    fn prepend_page_uses_before_cursor() {
        let store = FakeMailStore::volume(500);
        let first = store.list_page("inbox", ThreadFilter::All, "", None, None, 20);
        let rest = store.list_page(
            "inbox",
            ThreadFilter::All,
            "",
            first.next.as_ref(),
            None,
            20,
        );
        let back = store.list_page("inbox", ThreadFilter::All, "", None, rest.prev.as_ref(), 20);
        assert_eq!(
            back.threads.first().map(|t| t.id.as_str()),
            first.threads.first().map(|t| t.id.as_str())
        );
    }

    #[test]
    fn create_folder_rejects_system_names() {
        let mut store = FakeMailStore::seeded();
        assert_eq!(
            store.create_folder("Inbox"),
            Err(CreateFolderError::SystemName)
        );
        assert_eq!(
            store.create_folder(" trash "),
            Err(CreateFolderError::SystemName)
        );
        assert!(store
            .custom_folders()
            .all(|f| f.id.as_str() != "inbox" && f.id.as_str() != "trash"));
    }

    #[test]
    fn create_folder_adds_unique_label() {
        let mut store = FakeMailStore::seeded();
        let id = store.create_folder("Ideas").unwrap();
        assert_eq!(id.as_str(), "ideas");
        assert!(store.custom_folders().any(|f| f.label == "Ideas"));
        assert_eq!(
            store.create_folder("ideas"),
            Err(CreateFolderError::Duplicate)
        );
    }

    #[test]
    fn unread_count_matches_listed_unread() {
        let store = FakeMailStore::seeded();
        for folder in ["inbox", "work", "starred", "drafts"] {
            let listed = store
                .list_threads(folder, ThreadFilter::All, "")
                .iter()
                .filter(|t| t.unread())
                .count() as u32;
            assert_eq!(store.unread_count(folder), listed, "folder {folder}");
        }
    }

    #[test]
    fn uncached_is_skeleton_not_empty_copy() {
        let store = FakeMailStore::uncached();
        assert!(store.cache_empty());
        let page = store.list_page("inbox", ThreadFilter::All, "", None, None, LIST_PAGE);
        assert!(page.threads.is_empty());
        assert_eq!(page.total, 0);
        assert_eq!(store.empty("inbox", "").title, "You're all caught up.");
    }

    #[test]
    fn arrive_appends_without_replacing() {
        let mut store = FakeMailStore::uncached();
        store.arrive([thread(
            "arrive-1",
            "inbox",
            addr("A", "a@example.com"),
            "me",
            "First page",
            "hello",
            at(10, 0, 0),
            true,
            false,
            &["Inbox"],
            false,
            "<p>hello</p>",
        )]);
        assert!(!store.cache_empty());
        assert_eq!(
            store
                .list_page("inbox", ThreadFilter::All, "", None, None, 10)
                .threads
                .len(),
            1
        );
        store.arrive([thread(
            "arrive-2",
            "inbox",
            addr("B", "b@example.com"),
            "me",
            "Second page",
            "more",
            at(9, 0, 0),
            false,
            false,
            &["Inbox"],
            false,
            "<p>more</p>",
        )]);
        let page = store.list_page("inbox", ThreadFilter::All, "", None, None, 10);
        assert_eq!(page.threads.len(), 2);
        assert_eq!(page.threads[0].id.as_str(), "arrive-1");
        assert_eq!(page.threads[1].id.as_str(), "arrive-2");
    }

    #[test]
    fn empty_cache_ready_uses_folder_copy() {
        let mut store = FakeMailStore::uncached();
        store.arrive(std::iter::empty());
        assert!(!store.cache_empty());
        assert!(store
            .list_threads("inbox", ThreadFilter::All, "")
            .is_empty());
        assert_eq!(store.empty("inbox", "").title, "You're all caught up.");
        assert_eq!(store.empty("sent", "").title, "No sent messages yet.");
        assert_eq!(store.empty("spam", "").title, "No messages.");
        assert_eq!(store.empty("inbox", "from:zzz").title, "No messages found.");
    }

    #[test]
    fn seeded_spam_is_empty_with_cache() {
        let store = FakeMailStore::seeded();
        assert!(!store.cache_empty());
        assert!(store.list_threads("spam", ThreadFilter::All, "").is_empty());
        assert_eq!(store.empty("spam", "").title, "No messages.");
    }

    #[test]
    fn switch_account_filters_list_and_folders() {
        let mut store = FakeMailStore::seeded();
        assert_eq!(store.account().email, "john.doe@example.com");
        assert_eq!(store.unread_count("inbox"), 12);
        assert!(store
            .list_threads("inbox", ThreadFilter::All, "")
            .iter()
            .any(|t| t.id.as_str() == "notion-tos"));
        assert!(store.custom_folders().any(|f| f.id.as_str() == "work"));
        assert!(store.has_folder("work"));

        assert!(store.switch_account(ACCOUNT_JANE));
        assert_eq!(store.account().email, "jane.roe@example.com");
        assert_eq!(store.unread_count("inbox"), 1);
        let jane = store.list_threads("inbox", ThreadFilter::All, "");
        assert!(jane.iter().any(|t| t.id.as_str() == "jane-maya"));
        assert!(jane.iter().all(|t| t.id.as_str() != "notion-tos"));
        assert!(jane.iter().all(|t| t.account_id.as_str() == ACCOUNT_JANE));
        assert!(store.custom_folders().any(|f| f.id.as_str() == "clients"));
        assert!(store.custom_folders().all(|f| f.id.as_str() != "work"));
        assert!(!store.has_folder("work"));
        assert!(store
            .list_threads("inbox", ThreadFilter::All, "notion")
            .is_empty());
        assert!(store
            .list_threads("inbox", ThreadFilter::All, "maya")
            .iter()
            .any(|t| t.id.as_str() == "jane-maya"));

        assert!(store.switch_account(ACCOUNT_JOHN));
        assert!(store
            .list_threads("inbox", ThreadFilter::All, "")
            .iter()
            .any(|t| t.id.as_str() == "notion-tos"));
        assert!(store
            .list_threads("inbox", ThreadFilter::All, "")
            .iter()
            .all(|t| t.id.as_str() != "jane-maya"));
        assert_eq!(store.unread_count("inbox"), 12);
        assert!(store.has_folder("work"));
    }

    #[test]
    fn create_folder_stays_on_current_account() {
        let mut store = FakeMailStore::seeded();
        assert!(store.switch_account(ACCOUNT_JANE));
        store.create_folder("Secrets").unwrap();
        assert!(store.custom_folders().any(|f| f.label == "Secrets"));
        assert!(store.switch_account(ACCOUNT_JOHN));
        assert!(store.custom_folders().all(|f| f.label != "Secrets"));
        assert!(store.has_folder("work"));
    }

    #[test]
    fn unknown_account_does_not_switch() {
        let mut store = FakeMailStore::seeded();
        assert!(!store.switch_account("nope"));
        assert_eq!(store.account().id.as_str(), ACCOUNT_JOHN);
    }

    #[test]
    fn default_account_is_john() {
        let store = FakeMailStore::seeded();
        assert_eq!(store.default_account().id.as_str(), ACCOUNT_JOHN);
        assert_eq!(store.accounts().len(), 2);
    }

    #[test]
    fn add_account_switches_to_empty_inbox() {
        let mut store = FakeMailStore::seeded();
        let id = store.add_account("you@example.com").unwrap();
        assert_eq!(id.as_str(), "you");
        assert_eq!(store.account().email, "you@example.com");
        assert_eq!(store.account().status, AccountStatus::Syncing);
        assert!(store
            .list_threads("inbox", ThreadFilter::All, "")
            .is_empty());
        assert!(store.set_account_status("you", AccountStatus::Synced));
        assert_eq!(store.account().status, AccountStatus::Synced);
        assert_eq!(
            store.add_account("you@example.com"),
            Err(AddAccountError::Duplicate)
        );
        let clash = store.add_account("john@elsewhere.test").unwrap();
        assert_eq!(clash.as_str(), "john-2");
    }

    #[test]
    fn remove_account_wipes_only_that_account_from_the_fixture() {
        let mut store = FakeMailStore::seeded();
        assert!(store.switch_account(ACCOUNT_JOHN));
        let john_mail = store
            .list_threads("inbox", ThreadFilter::All, "")
            .iter()
            .any(|t| t.id.as_str() == "notion-tos");
        assert!(john_mail);

        assert!(store.remove_account(ACCOUNT_JANE));
        assert_eq!(store.accounts().len(), 1);
        assert!(store
            .accounts()
            .iter()
            .all(|a| a.id.as_str() != ACCOUNT_JANE));
        // John's account, mail, and folders are untouched.
        assert_eq!(store.account().id.as_str(), ACCOUNT_JOHN);
        assert!(store
            .list_threads("inbox", ThreadFilter::All, "")
            .iter()
            .any(|t| t.id.as_str() == "notion-tos"));
        assert!(store.has_folder("work"));
        // Jane's mail/folders are gone even though we never switched to her.
        assert!(store.switch_account(ACCOUNT_JOHN));
    }

    #[test]
    fn remove_account_of_the_current_account_falls_back_to_another() {
        let mut store = FakeMailStore::seeded();
        assert!(store.switch_account(ACCOUNT_JOHN));
        assert!(store.remove_account(ACCOUNT_JOHN));
        assert_eq!(store.accounts().len(), 1);
        assert_eq!(store.account().id.as_str(), ACCOUNT_JANE);
        assert!(store
            .list_threads("inbox", ThreadFilter::All, "")
            .iter()
            .all(|t| t.id.as_str() != "notion-tos"));
    }

    #[test]
    fn remove_account_unknown_id_is_a_no_op() {
        let mut store = FakeMailStore::seeded();
        assert!(!store.remove_account("nope"));
        assert_eq!(store.accounts().len(), 2);
    }

    #[test]
    fn rename_account_updates_display_name_only() {
        let mut store = FakeMailStore::seeded();
        assert!(store.rename_account(ACCOUNT_JOHN, "  Johnny  "));
        assert_eq!(store.account().name, "Johnny");
        assert_eq!(store.account().email, "john.doe@example.com");
        assert!(!store.rename_account(ACCOUNT_JOHN, "   "));
        assert_eq!(store.account().name, "Johnny");
        assert!(!store.rename_account("nope", "Someone"));
    }

    #[test]
    fn trash_drops_unread() {
        let mut store = FakeMailStore::seeded();
        let id = store
            .list_threads("inbox", ThreadFilter::Unread, "")
            .first()
            .expect("unread inbox")
            .id
            .as_str()
            .to_string();
        store.trash(std::slice::from_ref(&id), Some(&id));
        let t = store.get(&id).expect("trashed");
        assert!(t.deleted());
        assert!(!t.unread());
        store.mark_read(&id, false);
        assert!(!store.get(&id).expect("still there").unread());
        assert!(store
            .list_threads("trash", ThreadFilter::Unread, "")
            .is_empty());
    }
}
