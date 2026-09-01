//! Feather Mail GTK4/Relm4 shell. Data comes from `feathermail_core::Core`
//! (SQLite-backed) since T-074; `FakeMailStore` remains only for the
//! carved-out paths documented in `shell.rs` (Undo plus Theme/Density
//! settings).

#[macro_use]
mod bodylog;
mod html_view;
mod mail_writer;
mod msg;
mod nav;
mod rows;
mod secret_store;
mod selection;
mod settings_writer;
mod shell;

pub(crate) fn register_icon_resources() {
    let bytes = gtk4::glib::Bytes::from_static(include_bytes!(concat!(
        env!("OUT_DIR"),
        "/fm-icons.gresource"
    )));
    let resource = gtk4::gio::Resource::from_data(&bytes).expect("embedded Feather Mail icons");
    gtk4::gio::resources_register(&resource);

    if let Some(display) = gtk4::gdk::Display::default() {
        let theme = gtk4::IconTheme::for_display(&display);
        theme.add_resource_path("/app/feathermail/icons");
        // Uninstalled / cargo-run: hicolor lives in the tree, not yet in
        // /usr/share/icons. The dock still needs the XDG .desktop + theme
        // copy; this is what makes the window itself wear D1's mark.
        let bundled =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/icons");
        if bundled.is_dir() {
            theme.add_search_path(bundled);
        }
    }
}

fn main() {
    // T-067: the first startup milestone, taken before anything else this
    // process does, so the tap's own clock starts where the program does.
    shell::startup_mark("process");
    feathermail_security::init_tracing();
    let args = std::env::args().collect::<Vec<_>>();
    let mailto = args.iter().skip(1).find_map(|arg| parse_mailto(arg));
    let app = relm4::RelmApp::new("app.feathermail.FeatherMail");
    // Same name as Icon= in the .desktop file and the hicolor mark, so the
    // window, alt-tab and the dock all resolve D1's icon.png rather than a
    // generic missing-icon tile when the theme has the app installed.
    gtk4::Window::set_default_icon_name("app.feathermail.FeatherMail");
    if mailto.is_some() {
        // A mailto activation must reach Compose even when the main unique
        // instance is already running; the compose-only invocation remains
        // independent and shares the same durable Core profile.
        app.allow_multiple_instances(true);
    }
    app.with_args(vec![args
        .first()
        .cloned()
        .unwrap_or_else(|| "feathermail".into())])
        .run::<shell::App>(mailto);
}

fn parse_mailto(value: &str) -> Option<shell::MailtoDraft> {
    let url = url::Url::parse(value).ok()?;
    if url.scheme() != "mailto" {
        return None;
    }
    let mut draft = shell::MailtoDraft {
        to: url.path().to_string(),
        ..Default::default()
    };
    for (key, value) in url.query_pairs() {
        match key.as_ref().to_ascii_lowercase().as_str() {
            "to" => append_address(&mut draft.to, &value),
            "cc" => append_address(&mut draft.cc, &value),
            "bcc" => append_address(&mut draft.bcc, &value),
            "subject" => draft.subject = value.into_owned(),
            "body" => draft.body = value.into_owned(),
            _ => {}
        }
    }
    Some(draft)
}

fn append_address(target: &mut String, address: &str) {
    if !target.is_empty() && !address.is_empty() {
        target.push_str(", ");
    }
    target.push_str(address);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mailto_prefills_recipients_subject_and_body() {
        let draft = parse_mailto(
            "mailto:a@example.com?cc=b%40example.com&subject=Hello%20there&body=Hi%0Aall",
        )
        .unwrap();
        assert_eq!(draft.to, "a@example.com");
        assert_eq!(draft.cc, "b@example.com");
        assert_eq!(draft.subject, "Hello there");
        assert_eq!(draft.body, "Hi\nall");
    }
    /// T-101: one hairline per seam, and it is the token's colour.
    ///
    /// The owner: "линии сепараторов слишком яркие в тёмной теме". Two
    /// causes, both here. Adwaita paints `paned > separator` with a
    /// `background-image`, which covers a `background-color` -- the divider
    /// stayed the stock theme grey (measured #cdc7c2 on the light desk) no
    /// matter what `@hairline` said. And the panes on either side each drew
    /// a border of their own next to it, so every seam was two lines.
    #[test]
    fn one_hairline_per_seam_in_the_shell_colour() {
        let css = include_str!("style.css");
        let separator = css
            .split_once("paned > separator {")
            .expect("the paned divider is painted here")
            .1;
        let separator = &separator[..separator.find('}').unwrap()];
        assert!(
            separator.contains("background-color: @hairline;")
                && separator.contains("background-image: none;"),
            "an image sits over a background colour: without clearing it the              divider keeps the host theme's own grey"
        );
        let sidebar = css.split_once(".sidebar {").expect("sidebar").1;
        let sidebar = &sidebar[..sidebar.find('}').unwrap()];
        assert!(
            !sidebar.contains("border-right"),
            "the paned separator already draws this seam -- a border here              makes it two lines wide"
        );
        let list = css.split_once(".list-pane {").expect("list pane").1;
        let list = &list[..list.find('}').unwrap()];
        assert!(!list.contains("border-right"), "same seam, same reason");
    }

    /// T-138: the scrollbar is painted by this app, not by the host theme.
    ///
    /// Owner, on the dark theme: "скроллбар выглядит слишком светлым". A
    /// scrollbar the stylesheet never names keeps whatever the desktop theme
    /// draws, and Adwaita's light slider on a near-black pane is a bright bar
    /// down the side of the list. T-137 already makes GTK pick its dark
    /// variant; this makes the colour ours either way.
    #[test]
    fn the_scrollbar_is_painted_in_the_shell_tokens() {
        let css = include_str!("style.css");
        let slider = css
            .split_once("scrollbar slider {")
            .expect("the scrollbar slider is painted here")
            .1;
        let slider = &slider[..slider.find('}').unwrap()];
        assert!(
            slider.contains("background-color: @ink_tertiary;"),
            "the slider has to come out of the tokens, or it stays whatever              the host theme paints -- which is the bright bar the owner saw"
        );
        let indicator = css
            .split_once("scrollbar.overlay-indicator:not(.hovering):not(.dragging) slider {")
            .expect("the overlay indicator is answered on its own terms")
            .1;
        let indicator = &indicator[..indicator.find('}').unwrap()];
        assert!(
            indicator.contains("background-color: @ink_tertiary;"),
            "Adwaita styles the resting overlay bar with a more specific              selector, so the plain `scrollbar slider` rule never reaches it"
        );
        let trough = css
            .split_once("scrollbar trough {")
            .expect("the trough is painted here")
            .1;
        let trough = &trough[..trough.find('}').unwrap()];
        assert!(
            trough.contains("background-color: transparent;"),
            "a filled trough reads as a channel cut into the pane"
        );
    }

    /// T-101: tooltips are painted by this app, not by the host theme.
    ///
    /// A tooltip is its own toplevel, so nothing scoped under
    /// `window.fm-shell` reaches it. Left alone it was, on the owner's desk,
    /// a dark plate with near-black text on it: "подсказки при наведении на
    /// тёмном фоне и чёрным шрифтом. должны быть на светлом фоне".
    #[test]
    fn tooltips_use_the_shell_tokens() {
        let css = include_str!("style.css");
        let tooltip = css
            .split_once("tooltip.background,")
            .expect("tooltips are painted here")
            .1;
        let tooltip = &tooltip[..tooltip.find('}').unwrap()];
        assert!(
            tooltip.contains("background-color: @paper_pane;")
                && tooltip.contains("color: @ink;")
                && tooltip.contains("background-image: none;"),
            "paper under ink, and no theme image over it -- in the light              theme that is the light plate the owner asked for, and in the              dark theme it is a dark plate with light text rather than a              light one punched into it"
        );
    }

    #[test]
    fn css_tokens_named_from_design() {
        let light = include_str!("tokens_light.css");
        let dark = include_str!("tokens_dark.css");
        for token in [
            "--paper-sidebar",
            "--paper-pane",
            "--paper-recess",
            "--paper-wash",
            "--paper-selected",
            "--ink",
            "--accent",
            "--hairline",
            "--danger",
        ] {
            assert!(light.contains(token), "light missing {token}");
            assert!(dark.contains(token), "dark missing {token}");
        }
        assert!(light.contains("@define-color accent #1a64fc"));
        let css = include_str!("style.css");
        assert!(css.contains("btn-primary"));
        assert!(css.contains("welcome-card"));
        assert!(css.contains("settings-split"));
        assert!(css.contains("search-hit"));
        assert!(css.contains("empty-state"));
        assert!(css.contains("skeleton-list"));
        assert!(css.contains(".skel"));
        assert!(css.contains("account-pick"));
        assert!(css.contains("wizard-progress"));
        assert!(css.contains("wizard-error-slot"));
        assert!(css.contains("color: @danger;"));
        assert!(css.contains(
            ".display {\n  font-size: calc(26px * var(--fm-scale));\n  font-weight: 650;"
        ));
        assert!(css.contains(
            ".field-label {\n  font-size: calc(13px * var(--fm-scale));\n  font-weight: 500;"
        ));
        assert!(
            css.contains("--fm-scale: 1;")
                && css.contains("window.fm-shell.scale-200 { --fm-scale: 2; }"),
            "T-116: Interface scale must change the factor that every font-size multiplies, \
             not a root em that descendant px then ignore"
        );
        assert!(
            !css.contains("font-size: 1.25em")
                && !css.contains("font-size: 2em")
                && !css.contains("font-size: 26px;"),
            "a bare px/em font-size on a descendant (or the old em scale classes) \
             is the setting that looked like it worked and did nothing"
        );
        assert!(
            !css.contains("font-weight: 600;"),
            "GTK typography must use the DESIGN.md 400/500/650 weight scale"
        );
        let shell = include_str!("shell.rs");
        let nav = include_str!("nav.rs");
        assert!(nav.contains("Connecting..."));
        assert!(nav.contains("Synchronizing..."));
        assert!(nav.contains("\"Ready\""));
        assert!(shell.contains("MailboxPreset::Google"));
        assert!(shell.contains("MailboxPreset::Microsoft"));
        assert!(shell.contains("MailboxPreset::Yandex"));
        assert!(shell.contains("Other IMAP account"));
        assert!(shell.contains("google_provider_mark"));
        assert!(shell.contains("microsoft_provider_mark"));
        assert!(shell.contains("fm-unread-symbolic"));
        assert_eq!(
            shell.matches("set_can_target: false,").count(),
            11,
            "button content and the pull-refresh overlay must never steal pointer clicks"
        );
        assert!(
            shell.contains("add_css_class: \"compose-button\"")
                && shell.contains("fm-compose-symbolic"),
            "Compose must remain a compact icon-and-label toolbar action"
        );
        assert!(
            shell.contains("add_css_class: \"filter-button\"")
                && shell.contains("fm-filter-symbolic"),
            "Filter must remain the quiet icon-and-label control from the approved preview"
        );
        for legacy_icon in [
            "\"mail-unread-symbolic\"",
            "\"document-edit-symbolic\"",
            "\"filter-symbolic\"",
            "\"view-more-symbolic\"",
            "\"mail-attachment-symbolic\"",
        ] {
            assert!(
                !shell.contains(legacy_icon),
                "primary shell controls must use the bundled FeatherMail icon set, not {legacy_icon}"
            );
        }
        assert!(
            shell.contains(
                "add_css_class: \"compose-button\",\n                                    set_valign: gtk::Align::Center,"
            ),
            "Compose must opt out of GtkBox cross-axis Fill or it expands to the full 80px topbar"
        );
        assert!(shell.contains("Open Inbox"));
        assert!(shell.contains("wizard-error-slot"));
        assert!(
            shell.contains("set_halign: gtk::Align::Fill") && shell.contains("set_hexpand: true"),
            "wizard error slot and label must fill the card width so short errors stay on one line"
        );
        assert!(
            shell.find("set_label: \"Back\"").unwrap()
                < shell.find("add_css_class: \"wizard-error-slot\"").unwrap(),
            "wizard errors must remain below the Back button"
        );
        assert!(shell.contains("request_notifications_portal"));
        for class in [
            "wizard-chooser-lede",
            "wizard-provider",
            "wizard-imap-form",
            "wizard-progress-spinner",
            "wizard-progress-label",
            "wizard-open-inbox",
            "wizard-error-label",
        ] {
            assert!(
                shell.contains(&format!("add_css_class: \"{class}\""))
                    || shell.contains(&format!("add_css_class(\"{class}\")")),
                "wizard child {class} needs a stable class for direct visibility sync"
            );
        }
        assert!(
            shell.contains("fn sync_wizard_visibility")
                && shell.contains("fn refresh_wizard_view")
                && shell.contains("sync_wizard_visibility(&self.window, &self.wizard)"),
            "in-place wizard transitions must synchronise visibility and allocation"
        );
        for screen in ["Welcome", "AddAccount"] {
            assert!(
                shell.contains(&format!("set_visible: model.screen == Screen::{screen},")),
                "the {screen} page must be shown directly from the screen model"
            );
        }
        assert!(
            shell
                .contains("set_visible: matches!(model.screen, Screen::Inbox | Screen::Settings),")
                && shell.contains("settings-scrim")
                && shell.contains("root.add_overlay(&shell)"),
            "Settings must be an in-window overlay over the Inbox"
        );
        assert!(
            !shell.contains("root_stack") && !shell.contains("gtk::Stack"),
            "root navigation must not reintroduce GtkStack snapshots"
        );
        assert!(
            shell.contains("glib::idle_add_local_once")
                && shell.contains("window.queue_allocate();")
                && shell.contains("window.queue_draw();"),
            "screen navigation must allocate after Relm4 applies page visibility"
        );
        for class in ["welcome-screen", "add-account-screen", "inbox-screen"] {
            assert!(
                shell.contains(&format!("add_css_class: \"{class}\",")),
                "every screen host needs a stable class for direct visibility sync"
            );
        }
        assert!(
            shell.contains("shell.add_css_class(\"settings-shell\")")
                && shell.contains("root.add_css_class(\"settings-overlay\")"),
            "Settings must retain the modal card/scrim styling in the main window"
        );
        assert!(
            shell.contains("fn sync_screen_visibility")
                && shell.contains("widget.set_visible(visible);"),
            "navigation must synchronise GTK page visibility immediately"
        );
        assert!(!shell.contains("Loading..."));
        assert!(!css.contains("Loading..."));
    }

    /// T-097(5): no focus ring anywhere, and nothing that quietly hands the
    /// job to the host theme instead.
    ///
    /// T-054 painted a shared 2px `@focus_ring` outline on every interactive
    /// surface. The owner's answer on the live desk was that the blue border
    /// has to go from everywhere, so the rules now read `outline: none`.
    ///
    /// Deleting them would not be the same thing. A GTK control with no
    /// `outline` declaration of ours takes the *host* theme's focus ring, in
    /// the host's accent -- the exact failure T-054 recorded on an
    /// orange-accented session, where an untouched compose field opened
    /// wearing what reads as a validation error. So the selectors stay and
    /// carry `none`; this test is what keeps them from drifting back to a
    /// colour, and from being deleted as dead weight.
    ///
    /// Mutation: put `outline: 2px solid @focus_ring` back on any of them ->
    /// this test is red.
    #[test]
    fn nothing_wears_a_focus_ring_and_nothing_falls_back_to_the_host_theme() {
        let css = include_str!("style.css");
        for selector in [
            "window.fm-shell button:focus,",
            "window.fm-shell entry:focus-within,",
            "window.fm-shell textview:focus,",
            "window.fm-shell listview:focus-visible,",
            "window.fm-shell .thread-list:focus-visible {",
            "button.btn-primary:focus {",
            ".search:focus-within {",
        ] {
            assert!(
                css.contains(selector),
                "`{selector}` must stay: it is what turns the host theme's \
                 own ring off, and deleting it trades a blue border for \
                 whatever colour the session is themed in"
            );
        }
        assert!(
            !css.contains("solid @focus_ring"),
            "no surface may wear the focus ring: the owner asked for the blue \
             border to be gone from everywhere"
        );
    }

    /// T-054: a `GtkEntry` never carries the keyboard focus itself -- GTK
    /// puts it on the `text` node inside the entry -- so a rule written as
    /// `entry:focus` matches nothing and the field silently falls back to
    /// whatever ring the host theme paints. That is not a cosmetic
    /// difference: on a session with an orange system accent the compose
    /// window opened with its empty, untouched `To` field wearing a frame
    /// that reads as a validation error.
    ///
    /// T-097(5) turned the ring off rather than recolouring it, which makes
    /// this selector matter *more*, not less: `entry:focus` would leave the
    /// host ring on every text field in the app.
    #[test]
    fn a_text_field_takes_the_focus_ring_through_focus_within() {
        let css = include_str!("style.css");
        assert!(
            css.contains("window.fm-shell entry:focus-within,"),
            "the entry rule must reach the entry, not its unreachable `focus` \
             state -- it is what keeps the host theme's ring off text fields"
        );
        assert!(
            css.contains(".compose-grid entry.compose-field:focus-within {"),
            "the compose field's accent underline must reach the focused field"
        );
        for line in css.lines() {
            let selector = line.trim_end_matches([' ', '{', ',']);
            assert!(
                !selector.ends_with("entry:focus"),
                "`{line}` can never match -- GTK holds the focus on the \
                 `text` node inside an entry, so the host theme's ring is \
                 what the user would see"
            );
        }
    }

    /// T-054: a borderless underlined field cannot wear the shared ring. The
    /// ring is drawn around the whole allocation, and a compose field's
    /// allocation is the full width of the row with no padding of its own --
    /// so the ring came out as a bare rectangle around an empty `To` field,
    /// which is what a validation error looks like. The accent underline the
    /// design draws is the focus indicator there.
    #[test]
    fn a_compose_field_shows_focus_as_an_underline_not_a_ring() {
        let css = include_str!("style.css");
        assert!(
            css.contains(
                "window.fm-shell .compose-grid entry.compose-field:focus-within {\n  outline: none;\n}"
            ),
            "the shared ring must be turned off where the underline takes over"
        );
        assert!(
            css.contains(".compose-grid entry.compose-field:focus-within {\n  border-bottom-color: @accent;\n}"),
            "and the underline must still be the accent colour"
        );
    }

    /// T-054: text must not run into the edge of the field it is typed in.
    /// Both of these had zero padding on the side the text grows towards: a
    /// query long enough ran under the "/" badge at the search pill's end,
    /// and a recipient long enough touched the compose window's frame.
    #[test]
    fn a_text_field_keeps_its_text_off_its_own_edge() {
        let css = include_str!("style.css");
        for rule in [".search entry {", ".compose-grid entry.compose-field {"] {
            let body = &css[css.find(rule).expect(rule) + rule.len()..];
            let body = &body[..body.find('}').expect("unterminated rule")];
            assert!(
                body.contains("padding-right: 8px;") || body.contains("padding: 0 12px 0 0;"),
                "`{rule}` leaves its text touching the field's edge:\n{body}"
            );
        }
    }

    /// T-054: the settings pane owns the horizontal padding, exactly as the
    /// approved preview has it -- `.settings-pane { padding: var(--space-xl) }`
    /// with rows padded only vertically. Written the other way round, with the
    /// padding on each row, every row's hairline ran the full width of the
    /// card and stopped flush against its edge.
    #[test]
    fn settings_rows_are_inset_by_the_pane_not_by_themselves() {
        let css = include_str!("style.css");
        assert!(
            css.contains(".settings-pane {\n  padding: 24px;\n}"),
            "the settings pane must carry the padding"
        );
        assert!(
            css.contains(".setting-row {\n  padding: 12px 0;"),
            "so a row is padded vertically only, and its hairline stops short \
             of the card's edge"
        );
        let shell = include_str!("shell.rs");
        assert!(
            shell.contains("pane.add_css_class(\"settings-pane\");"),
            "and the pane has to carry the class for any of that to apply"
        );
    }

    /// T-130: a link the reading pane draws in a plain-text body wears
    /// DESIGN.md's Link Blue in both themes. GTK paints label links with
    /// its own default otherwise, and that is the theme's colour, not the
    /// product's.
    #[test]
    fn a_link_in_the_letter_wears_the_design_link_token() {
        let css = include_str!("style.css");
        let rule = css
            .split_once(".letter link {")
            .expect("the letter's links need a rule of their own")
            .1;
        let body = &rule[..rule.find('}').unwrap_or(rule.len())];
        assert!(
            body.contains("color: @link;"),
            "the link colour is the token, not a literal, got {body}"
        );
        for tokens in [
            include_str!("tokens_light.css"),
            include_str!("tokens_dark.css"),
        ] {
            assert!(
                tokens.contains("@define-color link "),
                "both themes define the link token the letter asks for"
            );
        }
    }

    /// T-126: the thread's history is narrower than the message being
    /// read -- the owner asked for the collapsed blocks "a little
    /// narrower", and the width is what tells a stack of headers apart
    /// from the letter.
    #[test]
    fn the_collapsed_thread_history_is_inset_under_the_open_message() {
        let css = include_str!("style.css");
        let collapsed = css
            .split_once("button.thread-card.collapsed {")
            .expect("collapsed thread cards must have a rule")
            .1
            .split_once('}')
            .expect("and it must be closed")
            .0;
        assert!(
            collapsed.contains("margin-left: 12px;") && collapsed.contains("margin-right: 12px;"),
            "the collapsed cards are inset on both sides, or they are the             same width as the open one"
        );
        let expanded = css
            .split_once(".thread-card.expanded {")
            .expect("the open card must have a rule")
            .1
            .split_once('}')
            .expect("and it must be closed")
            .0;
        assert!(
            !expanded.contains("margin-left") && !expanded.contains("margin-right"),
            "and the open message keeps the full width of the pane"
        );
    }

    #[test]
    fn icon_png_is_embedded() {
        let bytes = include_bytes!("../../../icon.png");
        assert!(bytes.starts_with(b"\x89PNG"));
    }

    /// The dock matches the running window to the desktop file by app id
    /// and WM class, then paints `Icon=`. Both names are the same string
    /// as `RelmApp::new` / `set_default_icon_name`.
    #[test]
    fn desktop_file_maps_the_window_to_the_mark() {
        let desktop = include_str!("../../../packaging/app.feathermail.FeatherMail.desktop");
        assert!(
            desktop.contains("Icon=app.feathermail.FeatherMail"),
            "the launcher icon is D1's hicolor mark, not a generic mailbox"
        );
        assert!(
            desktop.contains("StartupWMClass=app.feathermail.FeatherMail"),
            "without the class the dock treats a cargo-run window as unknown"
        );
        let main = include_str!("main.rs");
        assert!(
            main.contains("set_default_icon_name(\"app.feathermail.FeatherMail\")")
                && main.contains("packaging/icons"),
            "the window must look up D1's mark by the same name, including \
             from the source tree when nothing is installed"
        );
    }

    #[test]
    fn provider_icons_are_embedded_pngs() {
        for bytes in [
            include_bytes!("../assets/providers/google.png").as_slice(),
            include_bytes!("../assets/providers/microsoft.png").as_slice(),
        ] {
            assert!(bytes.starts_with(b"\x89PNG"));
        }
    }

    #[test]
    fn bundled_sounds_are_mpeg() {
        for bytes in [
            include_bytes!("../../../sounds/receive.mp3").as_slice(),
            include_bytes!("../../../sounds/send.mp3").as_slice(),
            include_bytes!("../../../sounds/update.mp3").as_slice(),
        ] {
            assert!(bytes.len() > 100);
            assert_eq!(bytes[0], 0xff);
            assert_eq!(bytes[1] & 0xe0, 0xe0);
        }
    }

    #[test]
    fn binary_crate_compiles() {
        assert!(!env!("CARGO_PKG_VERSION").is_empty());
    }
}
