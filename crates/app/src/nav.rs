//! Shell screens and Esc/Back order (T-012). Pure so keyboard paths are tested
//! without spinning GTK.

/// D47: Welcome → Add account → Inbox. Notifications portal is not a screen.
#[allow(dead_code)] // read by tests; kept as the onboarding contract
pub const ONBOARDING_SCREENS: &[&str] = &["Welcome", "Add account", "Inbox"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    Welcome,
    AddAccount,
    Inbox,
    Settings,
}

impl Screen {
    pub fn title(self) -> &'static str {
        match self {
            Self::Welcome => "Feather Mail",
            Self::AddAccount => "Add account",
            Self::Inbox => "Feather Mail",
            Self::Settings => "Settings",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsPage {
    General,
    Appearance,
    Notifications,
    Privacy,
    Accounts,
    AiMcp,
    Shortcuts,
    About,
    Diagnostics,
}

impl SettingsPage {
    pub const ALL: &'static [Self] = &[
        Self::General,
        Self::Appearance,
        Self::Notifications,
        Self::Privacy,
        Self::Accounts,
        Self::AiMcp,
        Self::Shortcuts,
        Self::About,
        Self::Diagnostics,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Appearance => "Appearance",
            Self::Notifications => "Notifications",
            Self::Privacy => "Privacy",
            Self::Accounts => "Accounts",
            Self::AiMcp => "AI & MCP",
            Self::Shortcuts => "Shortcuts",
            Self::About => "About",
            Self::Diagnostics => "Diagnostics",
        }
    }
}

/// What Esc does. Compose (own window) beats settings; overlay beats toast.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EscapeAction {
    CloseCompose,
    GoInbox,
    GoWelcome,
    CloseSearch,
    HideToast,
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WizardStep {
    Providers,
    OtherForm,
    Connecting,
    Synchronizing,
    Ready,
}

impl WizardStep {
    pub fn label(self) -> &'static str {
        match self {
            Self::Connecting => "Connecting...",
            Self::Synchronizing => "Synchronizing...",
            Self::Ready => "Ready",
            Self::Providers | Self::OtherForm => "",
        }
    }
}

/// T-017 add-account wizard. Progress is on the same screen, not a fourth page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Wizard {
    pub step: WizardStep,
    pub inbox_ready: bool,
    pub error: Option<String>,
    pub busy: bool,
    pub notifications_asked: bool,
    pub account_id: Option<String>,
}

impl Default for Wizard {
    fn default() -> Self {
        Self {
            step: WizardStep::Providers,
            inbox_ready: false,
            error: None,
            busy: false,
            notifications_asked: false,
            account_id: None,
        }
    }
}

impl Wizard {
    pub fn reset(&mut self) {
        let asked = self.notifications_asked;
        *self = Self {
            notifications_asked: asked,
            ..Self::default()
        };
    }

    pub fn show_chooser(&self) -> bool {
        matches!(self.step, WizardStep::Providers | WizardStep::OtherForm)
    }

    pub fn show_form(&self) -> bool {
        self.step == WizardStep::OtherForm
    }

    pub fn show_progress(&self) -> bool {
        matches!(
            self.step,
            WizardStep::Connecting | WizardStep::Synchronizing | WizardStep::Ready
        )
    }

    pub fn can_open_inbox(&self) -> bool {
        self.inbox_ready
    }

    pub fn submit(&mut self, account_id: String) {
        self.error = None;
        self.busy = true;
        self.inbox_ready = false;
        self.account_id = Some(account_id);
        self.step = WizardStep::Connecting;
    }

    pub fn metadata_arrived(&mut self) {
        if self.step == WizardStep::Connecting {
            self.step = WizardStep::Synchronizing;
        }
        self.inbox_ready = true;
    }

    pub fn sync_finished(&mut self) {
        self.step = WizardStep::Ready;
        self.inbox_ready = true;
        self.busy = false;
    }

    pub fn fail(&mut self, message: impl Into<String>) {
        self.error = Some(message.into());
        self.busy = false;
        self.inbox_ready = false;
        self.account_id = None;
        self.step = WizardStep::OtherForm;
    }

    pub fn cancel_to_form(&mut self) {
        self.step = WizardStep::OtherForm;
        self.inbox_ready = false;
        self.busy = false;
        self.error = None;
        self.account_id = None;
    }
}

/// T-098: `has_account` is what keeps Welcome from becoming a dead end.
/// Welcome is the *no profile yet* screen: it shows one button, `Add
/// account`, and no way back. Reaching it with an account already on disk
/// -- Esc out of the wizard opened from the account menu -- reads as
/// "the app forgot my mailbox", and nothing on the screen says otherwise.
pub fn escape(
    screen: Screen,
    compose_open: bool,
    search_open: bool,
    toast_visible: bool,
    wizard_inbox_ready: bool,
    has_account: bool,
) -> EscapeAction {
    if compose_open {
        return EscapeAction::CloseCompose;
    }
    match screen {
        Screen::Settings => EscapeAction::GoInbox,
        Screen::AddAccount if wizard_inbox_ready || has_account => EscapeAction::GoInbox,
        Screen::AddAccount => EscapeAction::GoWelcome,
        Screen::Welcome if has_account => EscapeAction::GoInbox,
        Screen::Welcome => EscapeAction::None,
        Screen::Inbox => {
            if search_open {
                EscapeAction::CloseSearch
            } else if toast_visible {
                EscapeAction::HideToast
            } else {
                EscapeAction::None
            }
        }
    }
}

/// Letter shortcuts (j/k/c/…) only on Inbox, not while typing, not over compose.
pub fn letter_shortcuts_live(screen: Screen, typing: bool, compose_open: bool) -> bool {
    screen == Screen::Inbox && !typing && !compose_open
}

pub const SHORTCUTS: &[(&str, &str)] = &[
    ("c", "Compose"),
    ("r", "Reply"),
    ("a", "Reply all"),
    ("f", "Forward"),
    ("e", "Archive"),
    ("#", "Delete"),
    ("s", "Star"),
    ("u", "Mark unread"),
    ("j", "Next message"),
    ("k", "Previous message"),
    ("o", "Open"),
    ("/", "Search"),
    ("Esc", "Close"),
    ("Ctrl+Enter", "Send"),
    ("Ctrl+R", "Refresh"),
];

pub use feathermail_core::MarkReadMode;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Prefs {
    pub confirm_delete: bool,
    pub mark_read: MarkReadMode,
    pub notify_mail: bool,
    pub notify_sound: bool,
    pub block_pixels: bool,
    pub block_remote: bool,
    pub confirm_links: bool,
    pub prefer_plain: bool,
    pub reduce_motion: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            confirm_delete: true,
            mark_read: MarkReadMode::Immediate,
            notify_mail: true,
            notify_sound: false,
            block_pixels: true,
            block_remote: true,
            confirm_links: true,
            prefer_plain: false,
            reduce_motion: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrefKey {
    ConfirmDelete,
    NotifyMail,
    NotifySound,
    BlockPixels,
    BlockRemote,
    ConfirmLinks,
    PreferPlain,
    ReduceMotion,
}

impl Prefs {
    pub fn toggle(&mut self, key: PrefKey) {
        match key {
            PrefKey::ConfirmDelete => self.confirm_delete = !self.confirm_delete,
            PrefKey::NotifyMail => self.notify_mail = !self.notify_mail,
            PrefKey::NotifySound => self.notify_sound = !self.notify_sound,
            PrefKey::BlockPixels => self.block_pixels = !self.block_pixels,
            PrefKey::BlockRemote => self.block_remote = !self.block_remote,
            PrefKey::ConfirmLinks => self.confirm_links = !self.confirm_links,
            PrefKey::PreferPlain => self.prefer_plain = !self.prefer_plain,
            PrefKey::ReduceMotion => self.reduce_motion = !self.reduce_motion,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn esc_closes_compose_first() {
        assert_eq!(
            escape(Screen::Settings, true, true, true, false, false),
            EscapeAction::CloseCompose
        );
    }

    #[test]
    fn esc_leaves_settings_to_inbox() {
        assert_eq!(
            escape(Screen::Settings, false, false, false, false, false),
            EscapeAction::GoInbox
        );
    }

    #[test]
    fn esc_from_add_account_goes_welcome() {
        assert_eq!(
            escape(Screen::AddAccount, false, false, false, false, false),
            EscapeAction::GoWelcome
        );
    }

    #[test]
    fn esc_closes_wizard_after_first_metadata() {
        assert_eq!(
            escape(Screen::AddAccount, false, false, false, true, false),
            EscapeAction::GoInbox
        );
    }

    #[test]
    fn esc_closes_search_then_toast() {
        assert_eq!(
            escape(Screen::Inbox, false, true, true, false, false),
            EscapeAction::CloseSearch
        );
        assert_eq!(
            escape(Screen::Inbox, false, false, true, false, false),
            EscapeAction::HideToast
        );
        assert_eq!(
            escape(Screen::Inbox, false, false, false, false, false),
            EscapeAction::None
        );
    }

    #[test]
    fn welcome_esc_is_noop() {
        assert_eq!(
            escape(Screen::Welcome, false, false, false, false, false),
            EscapeAction::None
        );
    }

    /// T-098: the owner launched Feather Mail with one account configured and
    /// got the `Add account` screen. The way there was Esc out of the wizard:
    /// it landed on Welcome, and Welcome answered Esc with nothing at all.
    #[test]
    fn a_configured_profile_is_never_left_on_the_welcome_screen() {
        assert_eq!(
            escape(Screen::Welcome, false, false, false, false, true),
            EscapeAction::GoInbox
        );
        assert_eq!(
            escape(Screen::AddAccount, false, false, false, false, true),
            EscapeAction::GoInbox
        );
    }

    #[test]
    fn onboarding_has_no_fourth_screen() {
        assert_eq!(ONBOARDING_SCREENS, ["Welcome", "Add account", "Inbox"]);
        assert!(!ONBOARDING_SCREENS.contains(&"Notifications"));
        assert!(!ONBOARDING_SCREENS.contains(&"Tutorial"));
        assert!(!ONBOARDING_SCREENS.contains(&"Ready"));
    }

    #[test]
    fn wizard_close_after_first_metadata_page() {
        let mut w = Wizard::default();
        w.submit("you".into());
        assert_eq!(w.step, WizardStep::Connecting);
        assert!(!w.can_open_inbox());
        w.metadata_arrived();
        assert_eq!(w.step, WizardStep::Synchronizing);
        assert!(w.can_open_inbox());
        w.sync_finished();
        assert_eq!(w.step, WizardStep::Ready);
        assert_eq!(w.step.label(), "Ready");
    }

    #[test]
    fn letters_only_on_inbox() {
        assert!(letter_shortcuts_live(Screen::Inbox, false, false));
        assert!(!letter_shortcuts_live(Screen::Inbox, true, false));
        assert!(!letter_shortcuts_live(Screen::Settings, false, false));
        assert!(!letter_shortcuts_live(Screen::Welcome, false, false));
    }

    #[test]
    fn settings_pages_cover_tz_70() {
        let labels: Vec<_> = SettingsPage::ALL.iter().map(|p| p.label()).collect();
        for need in [
            "General",
            "Appearance",
            "Notifications",
            "Privacy",
            "Accounts",
            "AI & MCP",
            "Shortcuts",
            "About",
            "Diagnostics",
        ] {
            assert!(labels.contains(&need), "missing {need}");
        }
    }
}
