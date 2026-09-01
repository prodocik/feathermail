//! Typed settings in SQLite, autosave debounce 750 ms (T-009, D27/D28/D37/D48).

use std::collections::HashSet;

use feathermail_db::Database;
use rusqlite::params;

use crate::error::CoreError;
use crate::mailbox::normalize_image_domain;
use crate::model::{AccountId, Density, FolderId, MarkReadMode, Theme};
use crate::store::{sql_err, Core};

/// Same cadence as draft autosave (D43). Not every keystroke/toggle hits disk.
pub const SETTINGS_AUTOSAVE_MS: u64 = 750;

/// T-097(6): the sidebar width the shell opens with, and the floor and
/// ceiling a drag is clamped to.
///
/// The default is the width the sidebar had when Inbox was selected, which is
/// what the owner asked the pane to open at. Before T-097 the sidebar was a
/// plain box in a horizontal box, so it took its *natural* width -- and an
/// active nav item is 650 weight, which is wider than the same word at 400.
/// Selecting a folder therefore widened the whole left column, for no reason
/// a reader could see. A `GtkPaned` position is a number, so it does not move
/// when the text inside it does.
pub const DEFAULT_SIDEBAR_WIDTH: i32 = 300;
pub const MIN_SIDEBAR_WIDTH: i32 = 200;
pub const MAX_SIDEBAR_WIDTH: i32 = 640;

/// T-099: where the message list ends and the reading pane begins.
///
/// The same story as the sidebar one pane over, and reported by the owner in
/// the same words: the list pane was a plain box in a horizontal box, so it
/// took its natural width, and selecting a card changes what a card is made
/// of -- the hover strip, the unread dot going away, a snippet arriving where
/// there was none. The column therefore breathed under the reader for no
/// reason a reader could see. The default is the 440px the pane was pinned to
/// by CSS before this became a number.
pub const DEFAULT_LIST_WIDTH: i32 = 440;
pub const MIN_LIST_WIDTH: i32 = 360;
pub const MAX_LIST_WIDTH: i32 = 900;

/// D16: cache cap default 2 GiB.
pub const DEFAULT_CACHE_LIMIT_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// T-111: the attachment cache gets its own ceiling rather than sharing the
/// body one. The two caches fill from different actions -- bodies arrive on
/// their own while mail is read, attachments only when the owner asks for a
/// file -- and one shared number would let a single 500 MB download evict
/// thousands of bodies the owner never chose to give up. Same default as
/// bodies, and the same rule that 0 means "unset, use the default".
pub const DEFAULT_ATTACHMENT_CACHE_LIMIT_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// T-117: Show images remembers the sender domain. A cap so a jammed
/// click-path cannot grow the settings row without bound.
pub const MAX_ALLOWED_IMAGE_DOMAINS: usize = 500;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub mark_read: MarkReadMode,
    pub confirm_delete: bool,
    pub theme: Theme,
    pub density: Density,
    pub ui_scale_percent: u8,
    pub notify_mail: bool,
    pub notify_sound: bool,
    /// Account ids whose new-mail notifications are muted. Kept as ids,
    /// not display names, so renaming an account does not silently unmute it.
    pub muted_notification_accounts: Vec<AccountId>,
    pub block_pixels: bool,
    pub block_remote: bool,
    /// T-117: sender domains for which remote images load without another
    /// Show images click. Host of From, not the image CDN.
    pub allowed_image_domains: Vec<String>,
    pub confirm_links: bool,
    pub prefer_plain: bool,
    pub reduce_motion: bool,
    pub default_account: Option<AccountId>,
    pub default_folder: FolderId,
    /// T-127: the mailbox the reader had open when the window last closed.
    /// Their place, not a preference -- `default_account` above is the
    /// preference, and the two are allowed to disagree. `None`, or an
    /// account that no longer exists, opens the first mailbox.
    pub last_account: Option<AccountId>,
    /// T-127: true when that place was the merged view rather than one
    /// mailbox. Kept apart from `last_account` because the merged view is
    /// not an account and has no id to store.
    pub last_unified: bool,
    pub cache_limit_bytes: u64,
    /// T-111: budget for downloaded attachment files, swept separately from
    /// the body cache. See [`DEFAULT_ATTACHMENT_CACHE_LIMIT_BYTES`].
    pub attachment_cache_limit_bytes: u64,
    pub mcp_enabled: bool,
    pub launch_on_startup: bool,
    /// T-097(6): where the user last dragged the sidebar divider.
    pub sidebar_width: i32,
    /// T-099: where the user last dragged the list/reader divider.
    pub list_width: i32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mark_read: MarkReadMode::Immediate,
            confirm_delete: true,
            theme: Theme::Light,
            density: Density::Comfortable,
            ui_scale_percent: 100,
            notify_mail: true,
            notify_sound: false,
            muted_notification_accounts: Vec::new(),
            block_pixels: true,
            block_remote: true,
            allowed_image_domains: Vec::new(),
            confirm_links: true,
            prefer_plain: false,
            reduce_motion: false,
            default_account: None,
            default_folder: FolderId("inbox".into()),
            last_account: None,
            last_unified: false,
            cache_limit_bytes: DEFAULT_CACHE_LIMIT_BYTES,
            attachment_cache_limit_bytes: DEFAULT_ATTACHMENT_CACHE_LIMIT_BYTES,
            mcp_enabled: false,
            launch_on_startup: false,
            sidebar_width: DEFAULT_SIDEBAR_WIDTH,
            list_width: DEFAULT_LIST_WIDTH,
        }
    }
}

impl Settings {
    fn clamp(&mut self) {
        self.ui_scale_percent = self.ui_scale_percent.clamp(100, 200);
        if self.cache_limit_bytes == 0 {
            self.cache_limit_bytes = DEFAULT_CACHE_LIMIT_BYTES;
        }
        if self.attachment_cache_limit_bytes == 0 {
            self.attachment_cache_limit_bytes = DEFAULT_ATTACHMENT_CACHE_LIMIT_BYTES;
        }
        if self.default_folder.as_str().is_empty() {
            self.default_folder = FolderId("inbox".into());
        }
        // A stored 0 is what a profile written before this setting existed
        // looks like once something else has parsed it; treat it as unset
        // rather than as a sidebar dragged shut.
        if self.sidebar_width == 0 {
            self.sidebar_width = DEFAULT_SIDEBAR_WIDTH;
        }
        self.sidebar_width = self
            .sidebar_width
            .clamp(MIN_SIDEBAR_WIDTH, MAX_SIDEBAR_WIDTH);
        // Same reading of a stored 0 as the sidebar above: a profile written
        // before the divider existed, not a pane dragged shut.
        if self.list_width == 0 {
            self.list_width = DEFAULT_LIST_WIDTH;
        }
        self.list_width = self.list_width.clamp(MIN_LIST_WIDTH, MAX_LIST_WIDTH);
        self.allowed_image_domains =
            normalize_allowed_image_domains(std::mem::take(&mut self.allowed_image_domains));
    }
}

pub struct SettingsStore {
    settings: Settings,
    dirty_since: Option<u64>,
    /// What this handle believes the `settings` table already holds. A flush
    /// writes only the keys that drifted from it, so a second process that
    /// changed a different key keeps its value instead of being overwritten
    /// by our older in-memory copy of it.
    persisted: Vec<(&'static str, String)>,
}

impl SettingsStore {
    pub fn load(db: &Database) -> Result<Self, CoreError> {
        let mut settings = Settings::default();
        let mut stmt = db
            .conn()
            .prepare("SELECT key, value FROM settings")
            .map_err(sql_err)?;
        let rows = stmt
            .query_map([], |row| {
                let key: String = row.get(0)?;
                let value: String = row.get(1)?;
                Ok((key, value))
            })
            .map_err(sql_err)?;
        for row in rows {
            let (key, value) = row.map_err(sql_err)?;
            apply_key(&mut settings, &key, &value);
        }
        settings.clamp();
        let persisted = pairs_of(&settings);
        Ok(Self {
            settings,
            dirty_since: None,
            persisted,
        })
    }

    pub fn get(&self) -> &Settings {
        &self.settings
    }

    pub fn patch(&mut self, now_ms: u64, f: impl FnOnce(&mut Settings)) {
        f(&mut self.settings);
        self.settings.clamp();
        if self.dirty_since.is_none() {
            self.dirty_since = Some(now_ms);
        }
    }

    pub fn flush(&mut self, db: &Database) -> Result<(), CoreError> {
        let pairs = pairs_of(&self.settings);
        let changed = changed_pairs(&pairs, &self.persisted);
        if changed.is_empty() {
            self.dirty_since = None;
            return Ok(());
        }
        let tx = db.conn().unchecked_transaction().map_err(sql_err)?;
        for (key, value) in changed {
            tx.execute(
                "INSERT INTO settings (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
            .map_err(sql_err)?;
        }
        tx.commit().map_err(sql_err)?;
        self.persisted = pairs;
        self.dirty_since = None;
        Ok(())
    }

    /// Writes if dirty for at least [`SETTINGS_AUTOSAVE_MS`].
    pub fn maybe_flush(&mut self, db: &Database, now_ms: u64) -> Result<bool, CoreError> {
        let Some(since) = self.dirty_since else {
            return Ok(false);
        };
        if now_ms.saturating_sub(since) < SETTINGS_AUTOSAVE_MS {
            return Ok(false);
        }
        self.flush(db)?;
        Ok(true)
    }
}

fn pairs_of(s: &Settings) -> Vec<(&'static str, String)> {
    vec![
        ("mark_read", s.mark_read.as_str().into()),
        ("confirm_delete", bool_str(s.confirm_delete)),
        ("theme", s.theme.as_str().into()),
        ("density", s.density.as_str().into()),
        ("ui_scale_percent", s.ui_scale_percent.to_string()),
        ("notify_mail", bool_str(s.notify_mail)),
        ("notify_sound", bool_str(s.notify_sound)),
        (
            "muted_notification_accounts",
            s.muted_notification_accounts
                .iter()
                .map(AccountId::as_str)
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        ("block_pixels", bool_str(s.block_pixels)),
        ("block_remote", bool_str(s.block_remote)),
        ("allowed_image_domains", s.allowed_image_domains.join("\n")),
        ("confirm_links", bool_str(s.confirm_links)),
        ("prefer_plain", bool_str(s.prefer_plain)),
        ("reduce_motion", bool_str(s.reduce_motion)),
        (
            "default_account",
            s.default_account
                .as_ref()
                .map(|a| a.as_str().to_string())
                .unwrap_or_default(),
        ),
        ("default_folder", s.default_folder.as_str().into()),
        (
            "last_account",
            s.last_account
                .as_ref()
                .map(|a| a.as_str().to_string())
                .unwrap_or_default(),
        ),
        ("last_unified", bool_str(s.last_unified)),
        ("cache_limit_bytes", s.cache_limit_bytes.to_string()),
        (
            "attachment_cache_limit_bytes",
            s.attachment_cache_limit_bytes.to_string(),
        ),
        ("mcp_enabled", bool_str(s.mcp_enabled)),
        ("launch_on_startup", bool_str(s.launch_on_startup)),
        ("sidebar_width", s.sidebar_width.to_string()),
        ("list_width", s.list_width.to_string()),
    ]
}

/// The keys whose value differs from what the table is believed to hold.
fn changed_pairs<'a>(
    current: &'a [(&'static str, String)],
    persisted: &[(&'static str, String)],
) -> Vec<(&'static str, &'a str)> {
    current
        .iter()
        .filter(|(key, value)| {
            persisted
                .iter()
                .find(|(known, _)| known == key)
                .is_none_or(|(_, known)| known != value)
        })
        .map(|(key, value)| (*key, value.as_str()))
        .collect()
}

fn bool_str(v: bool) -> String {
    if v { "true" } else { "false" }.into()
}

fn parse_bool(v: &str) -> Option<bool> {
    match v {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    }
}

fn apply_key(s: &mut Settings, key: &str, value: &str) {
    match key {
        "mark_read" => {
            if let Ok(v) = value.parse() {
                s.mark_read = v;
            }
        }
        "confirm_delete" => {
            if let Some(v) = parse_bool(value) {
                s.confirm_delete = v;
            }
        }
        "theme" => {
            if let Ok(v) = value.parse() {
                s.theme = v;
            }
        }
        "density" => {
            if let Ok(v) = value.parse() {
                s.density = v;
            }
        }
        "ui_scale_percent" => {
            if let Ok(v) = value.parse() {
                s.ui_scale_percent = v;
            }
        }
        "notify_mail" => {
            if let Some(v) = parse_bool(value) {
                s.notify_mail = v;
            }
        }
        "notify_sound" => {
            if let Some(v) = parse_bool(value) {
                s.notify_sound = v;
            }
        }
        "sidebar_width" => {
            if let Ok(v) = value.parse() {
                s.sidebar_width = v;
            }
        }
        "list_width" => {
            if let Ok(v) = value.parse() {
                s.list_width = v;
            }
        }
        "muted_notification_accounts" => {
            s.muted_notification_accounts = value
                .lines()
                .filter(|id| !id.is_empty())
                .map(|id| AccountId(id.to_owned()))
                .collect();
        }
        "block_pixels" => {
            if let Some(v) = parse_bool(value) {
                s.block_pixels = v;
            }
        }
        "block_remote" => {
            if let Some(v) = parse_bool(value) {
                s.block_remote = v;
            }
        }
        "allowed_image_domains" => {
            s.allowed_image_domains = value.lines().map(str::to_owned).collect();
        }
        "confirm_links" => {
            if let Some(v) = parse_bool(value) {
                s.confirm_links = v;
            }
        }
        "prefer_plain" => {
            if let Some(v) = parse_bool(value) {
                s.prefer_plain = v;
            }
        }
        "reduce_motion" => {
            if let Some(v) = parse_bool(value) {
                s.reduce_motion = v;
            }
        }
        "default_account" => {
            s.default_account = if value.is_empty() {
                None
            } else {
                Some(AccountId(value.into()))
            };
        }
        "default_folder" => {
            if !value.is_empty() {
                s.default_folder = FolderId(value.into());
            }
        }
        "last_account" => {
            s.last_account = if value.is_empty() {
                None
            } else {
                Some(AccountId(value.into()))
            };
        }
        "last_unified" => {
            if let Some(v) = parse_bool(value) {
                s.last_unified = v;
            }
        }
        "cache_limit_bytes" => {
            if let Ok(v) = value.parse() {
                s.cache_limit_bytes = v;
            }
        }
        "attachment_cache_limit_bytes" => {
            if let Ok(v) = value.parse() {
                s.attachment_cache_limit_bytes = v;
            }
        }
        "mcp_enabled" => {
            if let Some(v) = parse_bool(value) {
                s.mcp_enabled = v;
            }
        }
        "launch_on_startup" => {
            if let Some(v) = parse_bool(value) {
                s.launch_on_startup = v;
            }
        }
        _ => {}
    }
}

fn normalize_allowed_image_domains(raw: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for value in raw {
        let Some(domain) = normalize_image_domain(&value) else {
            continue;
        };
        if !seen.insert(domain.clone()) {
            continue;
        }
        out.push(domain);
        if out.len() == MAX_ALLOWED_IMAGE_DOMAINS {
            break;
        }
    }
    out.sort();
    out
}

impl Core {
    pub fn settings(&self) -> &Settings {
        self.settings.get()
    }

    pub fn patch_settings(&mut self, now_ms: u64, f: impl FnOnce(&mut Settings)) {
        self.settings.patch(now_ms, f);
    }

    pub fn flush_settings(&mut self) -> Result<(), CoreError> {
        self.settings.flush(&self.db)
    }

    pub fn maybe_autosave_settings(&mut self, now_ms: u64) -> Result<bool, CoreError> {
        self.settings.maybe_flush(&self.db, now_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Core;

    #[test]
    fn defaults_match_decisions() {
        let s = Settings::default();
        assert_eq!(s.mark_read, MarkReadMode::Immediate);
        assert!(s.confirm_delete);
        assert_eq!(s.density, Density::Comfortable);
        assert!(!s.notify_sound);
        assert!(s.notify_mail);
        assert!(s.block_pixels);
        assert!(s.block_remote);
        assert!(
            !s.prefer_plain,
            "T-141: a multipart/alternative shows its sanitized HTML half -- the \
             plain one loses the images and the links the sender put there"
        );
        assert_eq!(s.cache_limit_bytes, DEFAULT_CACHE_LIMIT_BYTES);
        assert!(!s.mcp_enabled);
        assert_eq!(s.ui_scale_percent, 100);
    }

    /// T-099: the divider between the list and the reader is a stored number
    /// like the sidebar's, so it survives a restart -- and a profile written
    /// before it existed opens at the width the pane used to be pinned to
    /// rather than shut.
    #[test]
    fn the_list_divider_is_remembered_and_a_missing_one_opens_at_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            let mut core = Core::open(&path).unwrap();
            assert_eq!(core.settings().list_width, DEFAULT_LIST_WIDTH);
            core.patch_settings(0, |s| s.list_width = 620);
            core.flush_settings().unwrap();
        }
        assert_eq!(Core::open(&path).unwrap().settings().list_width, 620);

        let mut older = Settings {
            list_width: 0,
            ..Default::default()
        };
        older.clamp();
        assert_eq!(older.list_width, DEFAULT_LIST_WIDTH);

        let mut dragged_out = Settings {
            list_width: 4000,
            ..Default::default()
        };
        dragged_out.clamp();
        assert_eq!(dragged_out.list_width, MAX_LIST_WIDTH);
    }

    /// T-127: the owner closed the app on one mailbox and it opened on
    /// another: "the app does not save which account was open before
    /// closing and opens the default account every time". The place is
    /// stored next to the preference and never confused with it.
    #[test]
    fn the_open_mailbox_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            let mut core = Core::open(&path).unwrap();
            assert_eq!(core.settings().last_account, None);
            assert!(!core.settings().last_unified);
            core.patch_settings(0, |s| {
                s.default_account = Some(AccountId("preferred".into()));
                s.last_account = Some(AccountId("where-they-were".into()));
            });
            core.flush_settings().unwrap();
        }
        {
            let reopened = Core::open(&path).unwrap();
            assert_eq!(
                reopened
                    .settings()
                    .last_account
                    .as_ref()
                    .map(|a| a.as_str()),
                Some("where-they-were"),
                "the window opens where the reader left it"
            );
            assert_eq!(
                reopened
                    .settings()
                    .default_account
                    .as_ref()
                    .map(|a| a.as_str()),
                Some("preferred"),
                "and the preference is a different fact, kept apart"
            );
        }

        let mut core = Core::open(&path).unwrap();
        core.patch_settings(0, |s| {
            s.last_account = None;
            s.last_unified = true;
        });
        core.flush_settings().unwrap();
        let reopened = Core::open(&path).unwrap();
        assert_eq!(reopened.settings().last_account, None);
        assert!(
            reopened.settings().last_unified,
            "the merged view is a place too, and it has no account id to store"
        );
    }

    #[test]
    fn restart_restores_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            let mut core = Core::open(&path).unwrap();
            core.patch_settings(0, |s| {
                s.mark_read = MarkReadMode::Manual;
                s.confirm_delete = false;
                s.density = Density::Compact;
                s.theme = Theme::Dark;
                s.block_pixels = false;
                s.block_remote = false;
                s.confirm_links = false;
                s.prefer_plain = true;
                s.notify_sound = true;
                s.muted_notification_accounts = vec![AccountId("quiet".into())];
                s.allowed_image_domains = vec!["News.Example.COM".into(), "example.com".into()];
                s.ui_scale_percent = 150;
                s.mcp_enabled = true;
                s.default_account = Some(AccountId("john".into()));
            });
            core.flush_settings().unwrap();
        }
        let core = Core::open(&path).unwrap();
        let s = core.settings();
        assert_eq!(s.mark_read, MarkReadMode::Manual);
        assert!(!s.confirm_delete);
        assert_eq!(s.density, Density::Compact);
        assert_eq!(s.theme, Theme::Dark);
        assert!(!s.block_pixels);
        assert!(!s.block_remote);
        assert!(!s.confirm_links);
        assert!(s.prefer_plain);
        assert!(s.notify_sound);
        assert_eq!(
            s.muted_notification_accounts,
            vec![AccountId("quiet".into())]
        );
        assert_eq!(
            s.allowed_image_domains,
            vec!["example.com".to_string(), "news.example.com".into()],
            "T-117: remembered sender domains survive restart, lowercased and unique"
        );
        assert_eq!(s.ui_scale_percent, 150);
        assert!(s.mcp_enabled);
        assert_eq!(s.default_account.as_ref().map(|a| a.as_str()), Some("john"));
    }

    #[test]
    fn a_flush_writes_only_what_this_handle_changed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        // Two handles on the same profile — the window and, say, the MCP
        // server. Each changes its own key; neither may undo the other.
        let mut window = Core::open(&path).unwrap();
        let mut server = Core::open(&path).unwrap();

        window.patch_settings(0, |s| s.last_unified = true);
        window.flush_settings().unwrap();

        server.patch_settings(0, |s| s.mcp_enabled = true);
        server.flush_settings().unwrap();

        let reopened = Core::open(&path).unwrap();
        assert!(
            reopened.settings().last_unified,
            "the merged view stayed open: the second handle never touched that key"
        );
        assert!(reopened.settings().mcp_enabled);
    }

    #[test]
    fn a_flush_that_changes_nothing_touches_no_row() {
        let pairs = pairs_of(&Settings::default());
        assert!(changed_pairs(&pairs, &pairs).is_empty());
        let mut moved = Settings::default();
        moved.last_unified = !moved.last_unified;
        let moved = pairs_of(&moved);
        let changed = changed_pairs(&moved, &pairs);
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].0, "last_unified");
    }

    #[test]
    fn debounce_skips_disk_until_750ms() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            let mut core = Core::open(&path).unwrap();
            core.patch_settings(0, |s| s.density = Density::Compact);
            assert!(!core.maybe_autosave_settings(749).unwrap());
        }
        let core = Core::open(&path).unwrap();
        assert_eq!(core.settings().density, Density::Comfortable);
        let mut core = Core::open(&path).unwrap();
        core.patch_settings(1_000, |s| s.density = Density::Compact);
        assert!(core
            .maybe_autosave_settings(1_000 + SETTINGS_AUTOSAVE_MS)
            .unwrap());
        drop(core);
        let core = Core::open(&path).unwrap();
        assert_eq!(core.settings().density, Density::Compact);
    }

    #[test]
    fn allowed_image_domains_drop_junk_and_dedupe() {
        let mut s = Settings {
            allowed_image_domains: vec![
                "Example.COM".into(),
                "bad host".into(),
                "example.com".into(),
                String::new(),
                ".hidden.com".into(),
            ],
            ..Default::default()
        };
        s.clamp();
        assert_eq!(s.allowed_image_domains, vec!["example.com".to_string()]);
    }

    /// T-141 turned the default the other way round, so the value that
    /// has to survive a reopen is the one the reader chose: ON.
    #[test]
    fn prefer_plain_choice_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            let mut core = Core::open(&path).unwrap();
            assert!(!core.settings().prefer_plain);
            core.patch_settings(0, |s| s.prefer_plain = true);
            core.flush_settings().unwrap();
        }
        let core = Core::open(&path).unwrap();
        assert!(core.settings().prefer_plain);
    }

    #[test]
    fn junk_keys_and_scale_do_not_break_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        let core = Core::open(&path).unwrap();
        core.db
            .conn()
            .execute(
                "INSERT INTO settings (key, value) VALUES ('nope', 'xyz'), ('ui_scale_percent', '9'), ('mark_read', 'nope')",
                [],
            )
            .unwrap();
        drop(core);
        let core = Core::open(&path).unwrap();
        assert_eq!(core.settings().ui_scale_percent, 100);
        assert_eq!(core.settings().mark_read, MarkReadMode::Immediate);
    }
}
