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

/// T-167: the wizard has three levels, not one screen with everything on
/// it. Level one asks *where the mailbox lives* -- an IMAP server we sign
/// into ourselves, or an account the desktop session already holds. Level
/// two is that answer's own list: the provider presets, or the session's
/// accounts. Level three is the detail: the IMAP form, then progress.
///
/// Before this the session accounts sat as a row above the presets, which
/// put two different kinds of sign-in -- "type a password" and "reuse the
/// one the system already has" -- in one undifferentiated column, and gave
/// the empty case nowhere to explain itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WizardStep {
    /// Level 1: IMAP or Linux.
    Source,
    /// Level 2, IMAP: Google / Microsoft / Yandex / other.
    Providers,
    /// Level 2, Linux: the session's own accounts, or how to add one.
    SystemAccounts,
    /// Level 3: the mailbox form.
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
            Self::Source | Self::Providers | Self::SystemAccounts | Self::OtherForm => "",
        }
    }

    /// The line under the title. It is per-step because each level asks a
    /// different question, and a single sentence covering all of them
    /// ("choose a preset or enter your settings") describes level two of
    /// one branch only.
    pub fn lede(self) -> &'static str {
        match self {
            Self::Source => "Where does this mailbox live?",
            Self::Providers => "Choose a preset or enter your mailbox settings.",
            Self::SystemAccounts => "Accounts you are already signed into on this desktop.",
            Self::OtherForm | Self::Connecting | Self::Synchronizing | Self::Ready => "",
        }
    }
}

/// Which level-one branch the wizard is in. It outlives the step because
/// a failure has to come back to the branch it started from: dropping a
/// refused session account onto the manual IMAP form would ask the owner
/// to type a password for an account whose whole point is not having one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WizardSource {
    #[default]
    Imap,
    System,
}

/// T-017 add-account wizard. Progress is on the same screen, not a fourth page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Wizard {
    pub step: WizardStep,
    pub source: WizardSource,
    pub inbox_ready: bool,
    pub error: Option<String>,
    pub busy: bool,
    pub notifications_asked: bool,
    pub account_id: Option<String>,
}

impl Default for Wizard {
    fn default() -> Self {
        Self {
            step: WizardStep::Source,
            source: WizardSource::Imap,
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

    /// Level 1 is on screen.
    pub fn show_source(&self) -> bool {
        self.step == WizardStep::Source
    }

    /// The preset list. It stays visible while the form below it is open:
    /// the form is level three *of the presets*, and hiding what you just
    /// picked from would make "Back" the only way to see it again.
    pub fn show_chooser(&self) -> bool {
        matches!(self.step, WizardStep::Providers | WizardStep::OtherForm)
    }

    /// Level 2 of the Linux branch -- the list, or the instructions when
    /// the session holds nothing we can use.
    pub fn show_system(&self) -> bool {
        self.step == WizardStep::SystemAccounts
    }

    /// Enter a branch. The step and the branch move together; nothing else
    /// may set one without the other, which is why these are methods.
    pub fn choose_imap(&mut self) {
        self.source = WizardSource::Imap;
        self.step = WizardStep::Providers;
        self.error = None;
    }

    pub fn choose_system(&mut self) {
        self.source = WizardSource::System;
        self.step = WizardStep::SystemAccounts;
        self.error = None;
    }

    /// What Back means on the level currently shown. `None` is "leave the
    /// wizard" -- the caller decides where to (Welcome or Inbox), which is
    /// a question about the profile, not about this screen.
    ///
    /// Progress is not handled here: cancelling a connection attempt has a
    /// generation to bump and an inbox to maybe open, so it stays with the
    /// caller and reaches this type through [`Wizard::cancel_to_form`].
    pub fn back_step(&self) -> Option<WizardStep> {
        match self.step {
            WizardStep::Source => None,
            WizardStep::Providers | WizardStep::SystemAccounts => Some(WizardStep::Source),
            WizardStep::OtherForm => Some(WizardStep::Providers),
            WizardStep::Connecting | WizardStep::Synchronizing | WizardStep::Ready => {
                Some(self.resume_step())
            }
        }
    }

    /// Where an interrupted or failed attempt lands. The IMAP branch owes
    /// the owner the form they filled in; the Linux branch owes them the
    /// list of accounts, because there is no form to return to.
    fn resume_step(&self) -> WizardStep {
        match self.source {
            WizardSource::Imap => WizardStep::OtherForm,
            WizardSource::System => WizardStep::SystemAccounts,
        }
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
        self.step = self.resume_step();
    }

    pub fn cancel_to_form(&mut self) {
        self.step = self.resume_step();
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

    /// T-167: the three levels, walked forward and back. The wizard opens
    /// on the question, not on one branch's answer.
    #[test]
    fn the_wizard_opens_on_the_source_question_and_back_leaves_it() {
        let wizard = Wizard::default();
        assert_eq!(wizard.step, WizardStep::Source);
        assert!(wizard.show_source());
        assert!(
            !wizard.show_chooser(),
            "no presets before a branch is picked"
        );
        assert!(!wizard.show_system());
        assert_eq!(
            wizard.back_step(),
            None,
            "Back on level one leaves the wizard entirely"
        );
    }

    #[test]
    fn each_branch_goes_back_to_the_source_question() {
        let mut imap = Wizard::default();
        imap.choose_imap();
        assert_eq!(imap.step, WizardStep::Providers);
        assert!(imap.show_chooser());
        assert_eq!(imap.back_step(), Some(WizardStep::Source));

        let mut system = Wizard::default();
        system.choose_system();
        assert_eq!(system.step, WizardStep::SystemAccounts);
        assert!(system.show_system());
        assert!(
            !system.show_chooser(),
            "the Linux branch is not the preset list with an extra row"
        );
        assert_eq!(system.back_step(), Some(WizardStep::Source));
    }

    /// Level three belongs to the IMAP branch, so Back from the form is
    /// the preset list -- one step, not out of the wizard.
    #[test]
    fn back_from_the_mailbox_form_returns_to_the_presets() {
        let mut wizard = Wizard::default();
        wizard.choose_imap();
        wizard.step = WizardStep::OtherForm;
        assert!(
            wizard.show_chooser(),
            "the presets stay visible above the form"
        );
        assert_eq!(wizard.back_step(), Some(WizardStep::Providers));
    }

    /// A refused sign-in must come back to the branch it started from.
    /// Dropping a rejected session account onto the IMAP form would ask
    /// for a password that account does not have.
    #[test]
    fn a_failure_returns_to_the_branch_it_started_in() {
        let mut imap = Wizard::default();
        imap.choose_imap();
        imap.submit("acc-1".into());
        imap.fail("Couldn't reach the server.");
        assert_eq!(imap.step, WizardStep::OtherForm);

        let mut system = Wizard::default();
        system.choose_system();
        system.submit("acc-2".into());
        system.fail("This account is no longer in Settings -> Online Accounts.");
        assert_eq!(system.step, WizardStep::SystemAccounts);
        assert_eq!(
            system.error.as_deref(),
            Some("This account is no longer in Settings -> Online Accounts.")
        );
    }

    /// Cancelling a connection attempt follows the same rule as failing.
    #[test]
    fn cancelling_a_connection_returns_to_the_branch_it_started_in() {
        let mut system = Wizard::default();
        system.choose_system();
        system.submit("acc-2".into());
        system.cancel_to_form();
        assert_eq!(system.step, WizardStep::SystemAccounts);
    }

    /// Reopening the wizard forgets the branch: the next account is not
    /// necessarily of the same kind as the last one.
    #[test]
    fn reset_returns_to_the_source_question() {
        let mut wizard = Wizard::default();
        wizard.choose_system();
        wizard.reset();
        assert_eq!(wizard.step, WizardStep::Source);
        assert_eq!(wizard.source, WizardSource::Imap);
    }

    /// Each level asks its own question, so each level has its own lede.
    #[test]
    fn every_chooser_level_has_its_own_lede() {
        assert!(!WizardStep::Source.lede().is_empty());
        assert!(!WizardStep::Providers.lede().is_empty());
        assert!(!WizardStep::SystemAccounts.lede().is_empty());
        assert_ne!(WizardStep::Source.lede(), WizardStep::Providers.lede());
        assert_ne!(
            WizardStep::Providers.lede(),
            WizardStep::SystemAccounts.lede()
        );
        assert_eq!(
            WizardStep::Connecting.lede(),
            "",
            "progress has its own label, not a lede"
        );
    }
}
