use std::cell::RefCell;
use std::rc::Rc;

use feathermail_core::{format_clock, Importance, ListRow, Thread};
use feathermail_html::decode_encoded_words;
use gtk::prelude::*;
use relm4::gtk;
use relm4::typed_view::list::RelmListItem;
use relm4::Sender;

use crate::msg::Msg;

pub struct MailRow {
    pub row: ListRow,
    /// T-099: whether a preview for this row is still on its way.
    ///
    /// An empty preview is not the same question as a loading one. The
    /// skeleton used to stand for both, so a row whose body had already been
    /// fetched -- and whose snippet was simply empty, or which sits past the
    /// warm-up's last message -- wore a preloader that would never resolve.
    /// A preloader that cannot finish is not a preloader; it is furniture.
    pub preview_pending: bool,
    /// Unix seconds the row's timestamp is measured against, taken once
    /// per painted page. The same clock `stamp_headers` stamps the date
    /// header with, so the two halves of one row cannot answer "when did
    /// this arrive" from different clocks -- and never a fixture constant,
    /// which pushed every real letter into `format_clock`'s "Today" branch.
    pub now: i64,
    /// T-161: the folder chip this row shows, or `None` for no chip.
    ///
    /// Decided by the shell (`App::row_chip_label`) and handed down as a
    /// value, exactly the way `now` is. It used to be derived here from
    /// `Thread.labels`, which `Core::map_thread` fills with an empty
    /// vector for every row it maps out of SQLite -- so the chip DESIGN.md
    /// promises was dead code that never painted once. What the reader
    /// actually needs it for is a list that mixes folders: Starred,
    /// Snoozed, the merged view. In an ordinary folder every row would say
    /// the same thing as the column heading, so the shell sends `None` and
    /// nothing is drawn.
    pub chip: Option<String>,
    pub sender: Sender<Msg>,
}

#[derive(Clone)]
pub struct MailRowWidgets {
    header: gtk::Label,
    thread: gtk::Box,
    sender_label: gtk::Label,
    time: gtk::Label,
    subject: gtk::Label,
    preview: gtk::Label,
    preview_skeleton: gtk::Box,
    star: gtk::Button,
    attach: gtk::Image,
    label_chip: gtk::Label,
    importance: gtk::Image,
    quick_actions: gtk::Box,
    quick_star: gtk::Button,
    id: Rc<RefCell<String>>,
    sender_slot: Rc<RefCell<Option<Sender<Msg>>>>,
}

impl RelmListItem for MailRow {
    type Root = gtk::Box;
    type Widgets = MailRowWidgets;

    fn setup(_item: &gtk::ListItem) -> (Self::Root, Self::Widgets) {
        let id = Rc::new(RefCell::new(String::new()));
        let sender_slot: Rc<RefCell<Option<Sender<Msg>>>> = Rc::new(RefCell::new(None));

        let header = gtk::Label::new(None);
        header.add_css_class("date-group");
        header.set_xalign(0.0);
        header.set_halign(gtk::Align::Start);

        let unread = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        unread.add_css_class("unread-dot");
        unread.set_valign(gtk::Align::Start);

        let importance = gtk::Image::from_icon_name("emblem-important-symbolic");
        importance.add_css_class("msg-importance");
        importance.set_pixel_size(12);
        importance.set_valign(gtk::Align::Start);
        importance.set_visible(false);
        importance.update_property(&[gtk::accessible::Property::Label("Important")]);

        let sender_label = gtk::Label::new(None);
        sender_label.add_css_class("msg-sender");
        sender_label.set_xalign(0.0);
        sender_label.set_hexpand(true);
        sender_label.set_ellipsize(gtk::pango::EllipsizeMode::End);

        let label_chip = gtk::Label::new(None);
        label_chip.add_css_class("chip");
        // A mailbox label is user-controlled server data. Keep it in the
        // row's reserved chip slot: without an ellipsized width it raises
        // the list pane's minimum width and pushes the other panes apart.
        label_chip.set_ellipsize(gtk::pango::EllipsizeMode::End);
        label_chip.set_max_width_chars(14);
        label_chip.set_single_line_mode(true);
        label_chip.set_visible(false);

        let attach = gtk::Image::from_icon_name("fm-attach-symbolic");
        attach.add_css_class("msg-attach");
        attach.set_pixel_size(16);
        attach.set_visible(false);
        attach.update_property(&[gtk::accessible::Property::Label("Has attachment")]);

        let time = gtk::Label::new(None);
        time.add_css_class("msg-time");

        let top = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        top.append(&sender_label);
        top.append(&label_chip);
        top.append(&attach);
        top.append(&time);

        let subject = gtk::Label::new(None);
        subject.add_css_class("msg-subject");
        subject.set_xalign(0.0);
        subject.set_single_line_mode(true);
        subject.set_lines(1);
        subject.set_ellipsize(gtk::pango::EllipsizeMode::End);

        let preview = gtk::Label::new(None);
        preview.add_css_class("msg-preview");
        preview.set_xalign(0.0);
        preview.set_single_line_mode(false);
        preview.set_lines(2);
        preview.set_wrap(true);
        preview.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        preview.set_ellipsize(gtk::pango::EllipsizeMode::End);
        // T-099: the slot is 34px whether or not there is text in it. Once
        // the skeleton stops standing in for "no preview at all", an empty
        // label is what holds the card's height -- and a card that changes
        // height when the text turns out to be empty is complaint (2) again.
        preview.set_size_request(-1, 34);

        // T-097(7): the preview slot when there is no preview text. Sync
        // stores headers; the snippet comes from a body, and a folder that
        // has just been synced has none of them yet. The two bars occupy the
        // same 34px the two text lines will, so the card is the same height
        // before and after the text arrives -- which is the whole of the
        // owner's second complaint ("first only the sender and half a
        // subject, then something loads in").
        let preview_skeleton = gtk::Box::new(gtk::Orientation::Vertical, 6);
        preview_skeleton.add_css_class("preview-skeleton");
        preview_skeleton.set_valign(gtk::Align::Center);
        for width in [-1, 168] {
            let bar = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            bar.add_css_class("skel");
            bar.set_size_request(width, 9);
            bar.set_halign(if width < 0 {
                gtk::Align::Fill
            } else {
                gtk::Align::Start
            });
            preview_skeleton.append(&bar);
        }

        // T-097(1): 2px, not 4px. The card's height is the sum of its slots,
        // and two 4px gaps were 8px of the 128px the owner asked to shrink.
        let main = gtk::Box::new(gtk::Orientation::Vertical, 2);
        main.set_hexpand(true);
        main.append(&top);
        main.append(&subject);
        main.append(&preview);
        main.append(&preview_skeleton);

        let star = gtk::Button::new();
        star.add_css_class("btn-icon");
        star.add_css_class("star");
        star.set_icon_name("fm-star-filled-symbolic");
        star.set_size_request(28, 28);
        star.set_valign(gtk::Align::Start);
        star.set_halign(gtk::Align::End);
        star.update_property(&[gtk::accessible::Property::Label("Star conversation")]);

        let marks = gtk::Box::new(gtk::Orientation::Vertical, 4);
        marks.append(&unread);
        marks.append(&importance);

        let thread = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        thread.add_css_class("msg-row");
        thread.append(&marks);
        thread.append(&main);
        let side = gtk::Box::new(gtk::Orientation::Vertical, 0);
        side.add_css_class("msg-side");
        side.set_valign(gtk::Align::Start);
        side.set_halign(gtk::Align::End);
        side.append(&star);
        thread.append(&side);

        // T-032 (D39): hover quick actions. The strip is an *overlay*
        // child over the whole row, not a sibling in the text flow, so
        // showing it never shifts sender/subject/preview. It starts
        // untargetable (GTK's `opacity: 0` does not stop clicks), and
        // `EventControllerMotion` flips both the CSS class and
        // `can_target` together. All of this is built once here in
        // `setup`; `bind` only writes data, so recycled rows never
        // accumulate handlers.
        let quick_actions = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        quick_actions.add_css_class("quick-actions");
        quick_actions.set_halign(gtk::Align::End);
        quick_actions.set_valign(gtk::Align::Start);
        // Stay clear of the list's scrollbar. The strip is an overlay over
        // the whole row and becomes targetable on hover, so without this it
        // is the thing under the pointer exactly where someone reaches for
        // the thumb -- the owner's "I can only scroll with the wheel". The
        // list's scrollbar is no longer an overlay either (see
        // `shell.rs`), and these two together mean the bar is both visible
        // and reachable.
        quick_actions.set_margin_end(QUICK_ACTIONS_SCROLLBAR_GAP);
        quick_actions.set_can_target(false);
        // T-068: hidden, not merely transparent. A strip that is `visible` is
        // measured, allocated and snapshotted on every frame of every row it
        // is in, and a page jump repaints a whole screen of rows at once: at
        // 10k that was a median of 13.7 ms per jump frame with 20 of 40 jumps
        // over the 16 ms budget, against 8.5 ms and 1-3 over once the four
        // buttons leave the layout until the pointer is actually on the row.
        // It costs nothing visible -- `opacity: 0` was already showing
        // nothing -- and it takes four buttons per row out of the
        // accessibility tree that no keyboard could reach anyway (the same
        // actions are on the context menu and the toolbar).
        quick_actions.set_visible(false);
        for (icon, tip, make) in [
            (
                "fm-archive-symbolic",
                "Archive",
                Msg::RowArchive as fn(String) -> Msg,
            ),
            (
                "fm-trash-symbolic",
                "Delete",
                Msg::RowDelete as fn(String) -> Msg,
            ),
            (
                "fm-read-symbolic",
                "Mark read",
                Msg::RowMarkRead as fn(String) -> Msg,
            ),
            (
                "fm-snooze-symbolic",
                "Snooze",
                Msg::RowSnooze as fn(String) -> Msg,
            ),
        ] {
            let btn = gtk::Button::new();
            btn.add_css_class("btn-icon");
            btn.set_icon_name(icon);
            btn.set_tooltip_text(Some(tip));
            btn.update_property(&[gtk::accessible::Property::Label(tip)]);
            let id_btn = id.clone();
            let sender_btn = sender_slot.clone();
            btn.connect_clicked(move |_| {
                let id = id_btn.borrow().clone();
                if id.is_empty() {
                    return;
                }
                if let Some(s) = sender_btn.borrow().as_ref() {
                    s.emit(make(id));
                }
            });
            quick_actions.append(&btn);
        }
        // T-099: the strip now sits where the star sits -- both 10px in from
        // the card's top-right corner, as asked -- so the plate covers the
        // row's own star while the pointer is on the row. The action cannot
        // simply go missing for exactly as long as the mouse is there, so the
        // strip carries a star of its own in the slot the covered one had.
        // It is the same `Msg::ToggleStar`, not a second door.
        let quick_star = gtk::Button::new();
        quick_star.add_css_class("btn-icon");
        quick_star.add_css_class("star");
        quick_star.set_icon_name("fm-star-symbolic");
        {
            let id_star = id.clone();
            let sender_star = sender_slot.clone();
            quick_star.connect_clicked(move |_| {
                let id = id_star.borrow().clone();
                if id.is_empty() {
                    return;
                }
                if let Some(s) = sender_star.borrow().as_ref() {
                    s.emit(Msg::ToggleStar(id));
                }
            });
        }
        quick_actions.append(&quick_star);

        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&thread));
        overlay.add_overlay(&quick_actions);

        let motion = gtk::EventControllerMotion::new();
        {
            let hover_on = overlay.clone();
            let quick_on = quick_actions.clone();
            let star_on = star.clone();
            motion.connect_enter(move |_, _, _| {
                // T-054: the class goes on the overlay, which is the one
                // widget here that contains *both* the row and the strip.
                // On `thread` it lit nothing: the strip is `thread`'s
                // overlay sibling, so `.msg-row.quick-on .quick-actions`
                // could never match it and the strip stayed at `opacity: 0`
                // for the whole life of the row. Hovering only made the
                // timestamp fade out and put nothing in its place -- the
                // pointer suite could press the buttons because pressing
                // goes by `can_target`, not by what is on screen.
                hover_on.add_css_class("quick-on");
                quick_on.set_visible(true);
                quick_on.set_can_target(true);
                // T-099: the plate is opaque and lands on top of the star.
                // Hiding the star is not cosmetic -- a widget under an
                // overlay still answers the pointer where the overlay has a
                // gap, and half a star peeking out from under a plate is the
                // kind of target nobody aims at on purpose.
                star_on.set_visible(false);
                // T-054 used to fade an unstarred star out under the strip
                // and then withdraw it from hit testing by hand, because
                // GTK's `opacity: 0` does not stop clicks and a press aimed
                // at the last quick action landed on a star nobody could see
                // -- the row snoozed instead of starring. Neither the fade
                // nor the `can_target` dance is back: `set_visible(false)`
                // takes the star out of the picture and out of hit testing in
                // one move, and the strip's own star is what answers while it
                // is gone.
            });
        }
        {
            let hover_off = overlay.clone();
            let quick_off = quick_actions.clone();
            let star_off = star.clone();
            motion.connect_leave(move |_| {
                hover_off.remove_css_class("quick-on");
                quick_off.set_visible(false);
                quick_off.set_can_target(false);
                star_off.set_visible(true);
            });
        }
        // T-054: the hover watch belongs to the overlay, not to `thread`.
        // The strip is an overlay *sibling* of `thread`, so with the
        // controller on `thread` the pointer reaching a quick action counted
        // as leaving the row: `leave` fired, `can_target` went back to false,
        // and the press that followed fell through the strip onto the row
        // underneath -- it selected the thread instead of archiving it, and
        // only sometimes, depending on which of the two arrived first. The
        // overlay contains both, so entering the strip is no longer leaving.
        overlay.add_controller(motion);

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&header);
        root.append(&overlay);

        // T-032: button 1 only -- the right button is the context menu
        // below. Clicks that land on a child button (the star) must not
        // also select the row; the hover strip lives outside this
        // subtree (overlay sibling), so its buttons never reach this
        // gesture at all.
        let click = gtk::GestureClick::new();
        click.set_button(1);
        let id_click = id.clone();
        let sender_click = sender_slot.clone();
        click.connect_released(move |gesture, _, x, y| {
            let on_button = gesture
                .widget()
                .and_then(|w| w.pick(x, y, gtk::PickFlags::DEFAULT))
                .is_some_and(widget_is_or_is_inside_button);
            if !row_click_selects(on_button) {
                return;
            }
            let id = id_click.borrow();
            if id.is_empty() {
                return;
            }
            if let Some(s) = sender_click.borrow().as_ref() {
                let modifiers = gesture.current_event_state();
                s.emit(Msg::SelectThreadGesture {
                    id: id.clone(),
                    ctrl: modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK),
                    shift: modifiers.contains(gtk::gdk::ModifierType::SHIFT_MASK),
                });
            }
        });
        thread.add_controller(click);

        // T-032: right click first selects the row (T-028's
        // mark-read-on-select policy applies, exactly like a left click),
        // then pops the short context menu.
        //
        // T-054: the menu itself is *not* a child of this row. Selecting a
        // thread rebinds its row -- `rebind_thread` removes the list item and
        // inserts a new one -- which disposes the widget this handler was
        // built on. A popover parented here therefore lost its parent in the
        // same frame it was told to pop up, and GTK answered with
        // `gtk_widget_realize() on a widget that isn't inside a toplevel`:
        // the right button did nothing at all, on every row, for as long as
        // this menu has existed. Nothing noticed because no test had ever
        // pressed the right button. The shell owns one menu parented to the
        // list itself (which no rebind touches) and pops it *after* the
        // selection it asked for, so ordering is not left to a timer.
        let context = gtk::GestureClick::new();
        context.set_button(3);
        {
            let id_ctx = id.clone();
            let sender_ctx = sender_slot.clone();
            let thread_ctx = thread.clone();
            context.connect_released(move |_, _, x, y| {
                let id = id_ctx.borrow().clone();
                if id.is_empty() {
                    return;
                }
                let Some(point) = context_menu_point(&thread_ctx, x, y) else {
                    return;
                };
                if let Some(s) = sender_ctx.borrow().as_ref() {
                    s.emit(Msg::RowContextMenu {
                        id,
                        x: point.0,
                        y: point.1,
                    });
                }
            });
        }
        thread.add_controller(context);

        let id_star = id.clone();
        let sender_star = sender_slot.clone();
        star.connect_clicked(move |_| {
            let id = id_star.borrow();
            if let Some(s) = sender_star.borrow().as_ref() {
                s.emit(Msg::ToggleStar(id.clone()));
            }
        });

        let widgets = MailRowWidgets {
            header,
            thread,
            sender_label,
            time,
            subject,
            preview,
            preview_skeleton,
            star,
            attach,
            label_chip,
            importance,
            quick_actions,
            quick_star,
            id,
            sender_slot,
        };
        // T-099: hand the live-repaint path a handle on this row. See
        // `repaint_live_row` for why a snippet landing must not go through
        // the model.
        LIVE_ROWS.with(|rows| rows.borrow_mut().push(widgets.clone()));
        (root, widgets)
    }

    fn bind(&mut self, widgets: &mut Self::Widgets, _root: &mut Self::Root) {
        *widgets.sender_slot.borrow_mut() = Some(self.sender.clone());
        match &self.row {
            ListRow::Header(label) => {
                widgets.header.set_visible(true);
                widgets.thread.set_visible(false);
                widgets.quick_actions.set_visible(false);
                widgets.header.set_label(label);
                widgets.id.replace(String::new());
            }
            ListRow::Thread(t) => {
                widgets.header.set_visible(false);
                widgets.thread.set_visible(true);
                widgets.quick_actions.set_visible(false);
                // T-099: a recycled row can arrive with the star hidden --
                // the pointer was over its previous tenant and `leave` never
                // fired for a widget that was reused underneath it. The strip
                // is put away one line above for the same reason.
                widgets.star.set_visible(true);
                bind_thread(
                    widgets,
                    t,
                    self.chip.as_deref(),
                    self.preview_pending,
                    self.now,
                );
            }
        }
    }
}

thread_local! {
    /// T-099: every row widget set the list factory has ever built, so a card
    /// already on screen can be repainted without going through the model.
    ///
    /// The entries are handles -- GTK widgets and the `Rc` slots they share
    /// with the live row -- so an entry always reflects what its row was last
    /// bound to. The list holds no more than GTK's recycling pool ever
    /// creates, which is the number of rows that fit on screen plus a few, so
    /// it is not pruned: dropping an entry would only cost the row its
    /// in-place repaint the next time it is recycled into view.
    static LIVE_ROWS: RefCell<Vec<MailRowWidgets>> = const { RefCell::new(Vec::new()) };
}

/// T-099: repaint one card that is already on screen, without touching the
/// model.
///
/// The obvious way to refresh a row -- `remove` + `insert` at its position --
/// is an `items-changed` on the store, and GTK answers that by rebuilding the
/// row widget: the old one is disposed, the list loses the widget that held
/// its focus, and the focus falls back to the first item, which the scrolled
/// window then scrolls into view. Measured on the nested stand: clicking a
/// card two thirds down the Inbox painted one frame at offset 0 and the next
/// back at 972 -- a full flash of the top of the folder, which is what the
/// owner reported as "something flickers above for a fraction of a second".
/// Putting the offset back afterwards cannot help: the wrong frame has
/// already been painted by then.
///
/// So nothing is removed. The row's own widgets are found by the thread id
/// they were last bound to and written through with `bind_thread` -- the very
/// function the factory's `bind` calls, so a live repaint and a fresh bind
/// cannot drift apart. Only mapped rows are considered: an unmapped entry is
/// a widget sitting in GTK's recycling pool, which will be bound from the
/// model before it is shown again. Returns whether a card was repainted.
pub fn repaint_live_row(t: &Thread, chip: Option<&str>, preview_pending: bool, now: i64) -> bool {
    LIVE_ROWS.with(|rows| {
        for w in rows.borrow().iter() {
            if w.thread.is_mapped() && w.id.borrow().as_str() == t.id.as_str() {
                bind_thread(w, t, chip, preview_pending, now);
                return true;
            }
        }
        false
    })
}

fn bind_thread(
    w: &MailRowWidgets,
    t: &Thread,
    chip: Option<&str>,
    preview_pending: bool,
    now: i64,
) {
    w.id.replace(t.id.as_str().to_string());
    let sender = display_sender(t);
    if t.message_count > 1 {
        w.sender_label
            .set_label(&format!("{sender} · {}", t.message_count));
    } else {
        w.sender_label.set_label(&sender);
    }
    w.time.set_label(&format_clock(t.date, now));
    w.subject.set_label(&display_subject(&t.subject));
    // T-097(7), T-099: text, or a preloader, or an empty slot -- exactly one
    // of the three, all 34px tall, so nothing on the card moves when the
    // preview arrives. The skeleton is the *loading* state and nothing else:
    // it needs both an empty preview and a fetch that is actually still out,
    // otherwise a row past the warm-up's reach (or one whose body simply has
    // no text to snip) would animate for ever with nothing coming.
    let preview = t.preview.trim();
    let loading = preview.is_empty() && preview_pending;
    w.preview.set_visible(!loading);
    w.preview_skeleton.set_visible(loading);
    w.preview.set_label(preview);
    w.thread
        .update_property(&[gtk::accessible::Property::Label(&format!(
            "{sender}: {}",
            display_subject(&t.subject)
        ))]);
    w.attach.set_visible(t.has_attachment);
    // T-161: the chip is the shell's answer, not this row's. See
    // `MailRow::chip`.
    if let Some(label) = chip {
        w.label_chip.set_visible(true);
        w.label_chip.set_label(label);
    } else {
        w.label_chip.set_visible(false);
    }
    w.importance.set_visible(t.importance == Importance::High);
    w.thread.remove_css_class("unread");
    if t.unread() {
        w.thread.add_css_class("unread");
    }
    // T-099: the selected row is the one GTK's selection model says it is,
    // and nothing else paints it. A `.selected` class written here at bind
    // time was a second answer to the same question, and it was the one that
    // went stale: moving the selection rebinds the row being opened, never
    // the one being left, so every row the owner clicked kept its highlight
    // and three clicks lit three cards at once.
    // T-099: the same state on both stars, under the two names their places
    // already use. The row's star is a lone control and says what it does to
    // what ("Unstar conversation"); the strip's is one of five in a plate
    // whose siblings are Archive/Delete/Mark read/Snooze, and a sixth reading
    // of the same string there would give the tree two nodes with one name.
    let (icon, row_label, strip_label) = if t.starred {
        ("fm-star-filled-symbolic", "Unstar conversation", "Unstar")
    } else {
        ("fm-star-symbolic", "Star conversation", "Star")
    };
    for (star, label) in [(&w.star, row_label), (&w.quick_star, strip_label)] {
        star.set_icon_name(icon);
        if t.starred {
            star.add_css_class("active");
        } else {
            star.remove_css_class("active");
        }
        star.set_tooltip_text(Some(label));
        star.update_property(&[gtk::accessible::Property::Label(label)]);
    }
}

/// T-097(1): the card names the sender by address, not by display name.
///
/// A display name is chosen by whoever sent the mail and is the half of a
/// From header that costs nothing to forge; the address is the part that had
/// to survive delivery. Two senders called "Support" are the same word on two
/// cards, and their addresses are not.
///
/// The name is not lost -- the reading pane header still shows it. Falls back
/// to the decoded name only when there is no address at all, which is a
/// malformed header rather than a normal message.
fn display_sender(t: &Thread) -> String {
    let email = t.from.email.trim();
    if email.is_empty() {
        decode_encoded_words(&t.from.name)
    } else {
        email.to_string()
    }
}

pub(crate) fn display_subject(subject: &str) -> String {
    let subject = decode_encoded_words(subject);
    // T-116: `\r` `\n` tab in a Subject draw as replacement glyphs and
    // wrap the card, which is the geometry T-097 promised would stay
    // fixed. Controls become spaces; the rest is one line.
    let cleaned: String = subject
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.is_empty() {
        "(No subject)".into()
    } else {
        cleaned
    }
}

/// T-032: one ghost button with a left-aligned label, shared by the
/// row/preview context menus and the More overflow menu so every short
/// menu in the shell looks the same.
pub(crate) fn menu_item(label: &str) -> gtk::Button {
    let item = gtk::Button::new();
    item.add_css_class("ghost");
    let text = gtk::Label::new(Some(label));
    text.set_xalign(0.0);
    text.set_hexpand(true);
    item.set_child(Some(&text));
    item
}

/// T-032: one context-menu entry — label plus the message factory
/// (Open reads the row/open id at click time, the rest are constants).
type ContextMenuItem = (&'static str, Box<dyn Fn() -> Msg>);

/// T-032: the one short context menu (ТЗ §32: «должно быть коротким»),
/// shared by the list rows and the reading-pane chrome so the two cannot
/// drift apart. Every item emits a selection-based `Msg` -- the caller
/// has already selected the row (or the pane already shows it), so the
/// ids match. No Move (T-036/T-038), no Copy sender/email (out of the
/// short list). Rows know no Core here: this file never names a Core
/// command, only `Msg`.
/// Room left at the row's right edge for the list's scrollbar, in pixels.
///
/// Wide enough for GTK's non-overlay bar plus its margin, so the hover
/// strip and the scrollbar never contend for the same pixels.
const QUICK_ACTIONS_SCROLLBAR_GAP: i32 = 16;

pub(crate) fn context_menu_box(
    sender_slot: Rc<RefCell<Option<Sender<Msg>>>>,
    open_id: Rc<RefCell<String>>,
    popover: &gtk::Popover,
) -> gtk::Box {
    let col = gtk::Box::new(gtk::Orientation::Vertical, 4);
    let items: [ContextMenuItem; 9] = [
        (
            "Open",
            Box::new(move || Msg::SelectThread(open_id.borrow().clone())),
        ),
        ("Reply", Box::new(|| Msg::Reply)),
        ("Forward", Box::new(|| Msg::Forward)),
        ("Mark as read", Box::new(|| Msg::MarkRead)),
        ("Star", Box::new(|| Msg::StarSelected)),
        ("Archive", Box::new(|| Msg::Archive)),
        ("Snooze", Box::new(|| Msg::Snooze)),
        ("Delete", Box::new(|| Msg::Delete)),
        ("Delete permanently", Box::new(|| Msg::PermanentDelete)),
    ];
    for (label, msg) in items {
        let item = menu_item(label);
        let slot = sender_slot.clone();
        let pop = popover.clone();
        item.connect_clicked(move |_| {
            if let Some(s) = slot.borrow().as_ref() {
                s.emit(msg());
            }
            pop.popdown();
        });
        col.append(&item);
    }
    col
}

/// T-054: where the right click landed, in the coordinate space of the
/// `GtkListView` the row lives in -- which is what the shell's one context
/// menu is parented to. Returns `None` when the row is not (yet) inside a
/// list view, which is the only state in which there is nothing to point at.
fn context_menu_point(row: &gtk::Box, x: f64, y: f64) -> Option<(f64, f64)> {
    let list = row.ancestor(gtk::ListView::static_type())?;
    let point = row.compute_point(&list, &gtk::graphene::Point::new(x as f32, y as f32))?;
    Some((point.x() as f64, point.y() as f64))
}

/// T-032: the row-select gesture ignores clicks that land on a child
/// button (the star; hover actions are overlay siblings and never reach
/// the gesture). Pure so it is testable without a display -- the GTK
/// `pick` in the gesture handler only computes the bool.
fn row_click_selects(is_button_target: bool) -> bool {
    !is_button_target
}

fn widget_is_or_is_inside_button(w: gtk::Widget) -> bool {
    let mut cur = Some(w);
    while let Some(widget) = cur {
        if widget.is::<gtk::Button>() {
            return true;
        }
        cur = widget.parent();
    }
    false
}

#[cfg(test)]
mod tests {
    /// Whole-file guards build their needles by concatenation: the test's
    /// own source would otherwise match them and the guard could never
    /// be green -- same reason `init_seeds_mark_read_from_core_settings`
    /// in shell.rs searches only the `init` body.
    fn joined(parts: &[&str]) -> String {
        parts.concat()
    }

    /// The one row-painting door, spelled out once: three contracts below
    /// anchor on it and rustfmt owns where its parameter list breaks.
    const BIND_THREAD_SIGNATURE: &str = concat!(
        "fn bind_thread(\n    w: &MailRowWidgets,\n    t: &Thread,\n",
        "    chip: Option<&str>,\n    preview_pending: bool,\n    now: i64,\n) {"
    );

    /// T-054 (D39): the hover watch belongs to the overlay that holds both
    /// the row and the strip. On `thread` alone, the pointer reaching a
    /// quick action counted as *leaving* the row: `leave` fired,
    /// `can_target` went back to false, and the press that followed fell
    /// through the strip onto the row underneath -- it selected the thread
    /// instead of archiving it, and only sometimes, depending on which
    /// event arrived first. Mutation: put the controller back on `thread`
    /// -> this test is red, and `clickthrough_hover_strip` selects instead
    /// of archiving.
    #[test]
    fn the_hover_watch_covers_the_strip_as_well_as_the_row() {
        let src = include_str!("rows.rs");
        let body = extract_brace_body(
            src,
            "fn setup(_item: &gtk::ListItem) -> (Self::Root, Self::Widgets) {",
        );
        assert!(
            body.contains("overlay.add_controller(motion);"),
            "the strip is an overlay sibling of the row: only the overlay \
             contains both, so only there is entering the strip not leaving \
             the row"
        );
        assert!(
            !body.contains("thread.add_controller(motion);"),
            "on the row alone, hovering a quick action ends the hover that \
             made it clickable"
        );
    }

    /// T-054 (T-032): the right button hands the shell a thread id and a
    /// point and stops there. The menu it used to own was parented to a
    /// widget `rebind_thread` disposes the moment the selection this
    /// gesture asks for goes through, so it lost its parent in the frame it
    /// was told to pop up and the right button did nothing at all, on every
    /// row, for as long as it had existed -- no test had ever pressed it.
    /// Mutation: build a `Popover` in `setup` again -> this test is red.
    #[test]
    fn the_row_only_reports_a_right_click_it_does_not_own_the_menu() {
        let src = include_str!("rows.rs");
        let body = extract_brace_body(
            src,
            "fn setup(_item: &gtk::ListItem) -> (Self::Root, Self::Widgets) {",
        );
        assert!(
            body.contains("Msg::RowContextMenu") && body.contains("context_menu_point("),
            "the row reports where the click landed, in the list's own \
             coordinates, and lets the shell pop its menu"
        );
        assert!(
            !body.contains(&joined(&["Popover", "::new"])),
            "a popover parented to a recycled row is disposed under the \
             click that opened it"
        );
    }

    fn extract_brace_body<'a>(src: &'a str, marker: &str) -> &'a str {
        let start = src
            .find(marker)
            .unwrap_or_else(|| panic!("{marker} must exist verbatim"));
        let body_start = start + marker.len();
        let mut depth = 1i32;
        let mut end = None;
        for (i, ch) in src[body_start..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(body_start + i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let end = end.unwrap_or_else(|| panic!("{marker} must have a matching closing brace"));
        &src[body_start..end]
    }

    /// T-032 (D39): a click whose target is a button (star) must not
    /// select the row; a click on the row itself still selects.
    #[test]
    fn row_click_selects_only_non_button_targets() {
        assert!(!super::row_click_selects(true));
        assert!(super::row_click_selects(false));
    }

    #[test]
    fn empty_subject_has_an_honest_visible_fallback() {
        assert_eq!(super::display_subject(""), "(No subject)");
        assert_eq!(super::display_subject("  \t"), "(No subject)");
        assert_eq!(super::display_subject("Project update"), "Project update");
        assert_eq!(
            super::display_subject("Line\r\none\twith\nbreaks"),
            "Line one with breaks",
            "control characters must not wrap or stretch the card"
        );
    }

    /// Fail-closed on virtualization: `bind` runs per recycled row, so
    /// any handler/controller created there would accumulate. All
    /// controllers, buttons, and the popover live in `setup`. Mutation:
    /// move a `connect_clicked` into `bind` -> this test is red.
    #[test]
    fn bind_only_writes_data_never_wires_handlers() {
        let src = include_str!("rows.rs");
        let body = extract_brace_body(
            src,
            "fn bind(&mut self, widgets: &mut Self::Widgets, _root: &mut Self::Root) {",
        );
        for forbidden in [
            "connect_clicked",
            &joined(&["GestureClick", "::new"]),
            "EventControllerMotion",
            &joined(&["Popover", "::new"]),
        ] {
            assert!(
                !body.contains(forbidden),
                "bind must not create {forbidden} -- handlers/controllers are \
                 setup-once, or recycled rows accumulate them"
            );
        }
        assert!(
            body.contains("bind_thread") && body.contains("id.replace"),
            "bind still writes labels/classes/id"
        );
    }

    /// Fail-closed on the hover strip: four row-id actions, a motion
    /// controller, and `can_target` gating all live in `setup`.
    /// Mutation: drop `set_can_target` -> invisible strip still eats
    /// clicks, and this test is red.
    #[test]
    fn setup_builds_the_hover_strip_once_with_target_gating() {
        let src = include_str!("rows.rs");
        let body = extract_brace_body(
            src,
            "fn setup(_item: &gtk::ListItem) -> (Self::Root, Self::Widgets) {",
        );
        for required in [
            "quick-actions",
            "RowArchive",
            "RowDelete",
            "RowMarkRead",
            "RowSnooze",
            "EventControllerMotion",
            "set_can_target",
            "Overlay",
        ] {
            assert!(
                body.contains(required),
                "setup must build {required} for the D39 hover strip"
            );
        }
    }

    /// T-097(4), and the shape T-054 was really about: a control must never
    /// be invisible while it still takes clicks.
    ///
    /// T-054 hit that by fading an unstarred star out under the hover strip
    /// and then withdrawing it from hit testing by hand -- GTK's `opacity: 0`
    /// does not stop clicks, so without the second half a press aimed at the
    /// last quick action landed on a star nobody could see and the row
    /// snoozed instead of starring. T-097 removed the first half instead: the
    /// star does not fade, so there is nothing to gate, and the plate stops
    /// short of the star column rather than covering it.
    ///
    /// Mutation: put the `.quick-on ... .star` fade back -> this test is red,
    /// and the invisible-but-clickable star is back with it.
    #[test]
    fn the_star_is_never_invisible_while_it_still_takes_clicks() {
        let css = include_str!("style.css");
        assert!(
            !css.contains(".star:not(.active)") && !css.contains(".quick-on button.btn-icon.star"),
            "nothing may fade the star while the strip is up: an `opacity: 0` \
             star still answers the mouse, and the press that reaches it was \
             aimed at a quick action"
        );
        let src = include_str!("rows.rs");
        let body = extract_brace_body(
            src,
            "fn setup(_item: &gtk::ListItem) -> (Self::Root, Self::Widgets) {",
        );
        assert!(
            !body.contains("star_on.set_can_target") && !body.contains("star_off.set_can_target"),
            "with the fade gone there is nothing for the hover handlers to \
             gate; a `can_target` dance here means the fade came back"
        );
        assert!(
            body.contains("star.set_size_request(28, 28)"),
            "the strip's right margin reserves a fixed star column -- the two \
             numbers have to be able to agree"
        );
    }

    /// T-068: an unhovered row must keep the strip out of the layout, not
    /// merely out of sight. Measured on 10k with `scripts/perf/scroll.py
    /// --key Next --pages 40`: median 13.7 ms per jump frame and 8-20 of 40
    /// jumps over the 16 ms budget with the strip always `visible`, against
    /// 8.5-9.7 ms and 1-3 over with it hidden until the pointer arrives.
    /// Mutation: `set_visible(true)` in `bind`'s thread arm -> this test is
    /// red, and the jump frames go back over budget.
    #[test]
    fn an_unhovered_row_keeps_the_strip_out_of_the_layout() {
        let src = include_str!("rows.rs");
        let bind = extract_brace_body(
            src,
            "fn bind(&mut self, widgets: &mut Self::Widgets, _root: &mut Self::Root) {",
        );
        assert!(
            !bind.contains(&joined(&["widgets.quick_actions.set_visible(", "true)"])),
            "binding a row must not put the strip into the layout: it is \
             invisible until the pointer arrives, and a page jump pays for \
             every widget a screenful of rows measures"
        );
        let setup = extract_brace_body(
            src,
            "fn setup(_item: &gtk::ListItem) -> (Self::Root, Self::Widgets) {",
        );
        assert!(
            setup.contains(&joined(&["quick_on.set_visible(", "true);"]))
                && setup.contains(&joined(&["quick_off.set_visible(", "false);"])),
            "the hover handlers are what put the strip in and out of the \
             layout, on the same signals that light and unlight it"
        );
    }

    /// T-054: the class that lights the strip has to sit on a widget the
    /// strip is *inside of*. On `thread` it lit nothing -- the strip is
    /// `thread`'s overlay sibling, so `.msg-row.quick-on .quick-actions`
    /// described a tree that does not exist and the strip stayed at
    /// `opacity: 0` for the life of the row. Hovering only faded the
    /// timestamp out and put nothing in its place. No pointer assertion could
    /// see this: pressing a quick action goes by `can_target`, which was
    /// correct all along, so the suite pressed buttons nobody could see.
    /// Mutation: put the class back on `thread` (and the `.msg-row` anchor
    /// back in style.css) -> this test is red, and a screenshot of a hovered
    /// row shows no strip.
    #[test]
    fn the_hover_class_goes_on_a_widget_that_contains_the_strip() {
        let src = include_str!("rows.rs");
        let body = extract_brace_body(
            src,
            "fn setup(_item: &gtk::ListItem) -> (Self::Root, Self::Widgets) {",
        );
        assert!(
            body.contains("let hover_on = overlay.clone();")
                && body.contains("hover_on.add_css_class(\"quick-on\")"),
            "the hover class must go on the overlay -- the one widget here \
             that contains both the row and the strip"
        );
        assert!(
            body.contains("let hover_off = overlay.clone();")
                && body.contains("hover_off.remove_css_class(\"quick-on\")"),
            "and leaving the row must take it off that same widget"
        );
        assert!(
            !body.contains("thread_on.add_css_class"),
            "on `thread` the class reaches the row but never the strip"
        );

        let css = include_str!("style.css");
        assert!(
            css.contains(".quick-on .quick-actions {"),
            "style.css must raise the strip from the class on its own \
             ancestor, not from one on the row beside it"
        );
        assert!(
            !css.contains(".msg-row.quick-on"),
            "a `.msg-row`-anchored selector cannot reach the strip: it is the \
             row's overlay sibling, not its child"
        );
    }

    /// The strip is an overlay over the *whole* row, so its right margin is
    /// the only thing keeping it off the star column: the row's own 16px
    /// padding plus the 28px star. With less, a starred row cannot be
    /// unstarred with the mouse at all -- the press lands on Snooze, which is
    /// what the pointer suite caught (T-054).
    ///
    /// T-097(4) moved the strip in from 64px to 52px (16 + 28 + an 8px gap)
    /// and gave it a plate; the floor is what the star needs, not what the
    /// old margin happened to be.
    /// T-099: three clicks lit three cards. The row highlight has exactly one
    /// source -- GTK's selection model -- because the bind-time class was
    /// only ever refreshed for the row being opened, never for the one being
    /// left behind.
    #[test]
    fn only_the_selection_model_paints_the_selected_row() {
        let css = include_str!("style.css");
        assert!(
            !css.contains(".msg-row.selected"),
            "a class-based highlight is a second answer to `which row is \
             selected`, and it is the one that goes stale"
        );
        assert!(
            css.contains("listview.thread-list > row:selected .msg-row"),
            "the selection model's own state is what must paint the card"
        );
        let src = include_str!("rows.rs");
        let bind = extract_brace_body(src, BIND_THREAD_SIGNATURE);
        assert!(
            !bind.contains(&joined(&["add_css_class(\"select", "ed\")"])),
            "nothing in a row may write the selection back onto the widget"
        );
    }

    /// T-099: a skeleton is a preloader, so it may only stand where
    /// something is actually loading -- and it has to move. A row past the
    /// warm-up's last message, or one whose body holds no text to snip, kept
    /// a still grey bar for ever and read as a card stuck mid-load.
    #[test]
    fn the_row_skeleton_means_loading_and_animates() {
        let src = include_str!("rows.rs");
        let bind = extract_brace_body(src, BIND_THREAD_SIGNATURE);
        assert!(
            bind.contains("let loading = preview.is_empty() && preview_pending;"),
            "an empty preview alone is not a loading preview"
        );
        assert!(
            bind.contains("w.preview_skeleton.set_visible(loading)")
                && bind.contains("w.preview.set_visible(!loading)"),
            "exactly one of the two holds the 34px slot"
        );
        let setup = extract_brace_body(
            src,
            "fn setup(_item: &gtk::ListItem) -> (Self::Root, Self::Widgets) {",
        );
        assert!(
            setup.contains("preview.set_size_request(-1, 34)"),
            "with the skeleton gone the empty label is what keeps the card's \
             height, so it has to be given one"
        );
        let css = include_str!("style.css");
        assert!(
            css.contains("@keyframes skel-pulse") && css.contains("animation: skel-pulse"),
            "a preloader that does not move is just a grey box"
        );
        assert!(
            css.contains("window.fm-shell.reduce-motion .skel"),
            "Reduce motion must reach the pulse, not only transitions"
        );
    }

    /// T-099: the strip is pinned 10px in from the card's top-right corner,
    /// which is where the star also sits -- the owner asked for both. The
    /// margins count from the overlay, which spans the card's own 12px
    /// inset, so 10px in reads as 22. The consequence is that the plate now
    /// covers the star, and this test holds the two halves of the answer to
    /// that together: the covered star is hidden (not left invisible-but-
    /// clickable, T-054's bug) and the strip carries a star of its own, so
    /// the action does not vanish for as long as the pointer is on the row.
    #[test]
    fn the_hover_strip_sits_ten_pixels_in_and_keeps_the_star_reachable() {
        let css = include_str!("style.css");
        let body = css
            .split_once(".quick-actions {")
            .expect("style.css must style .quick-actions")
            .1
            .split_once('}')
            .expect(".quick-actions block must close")
            .0;
        let px = |name: &str| {
            body.lines()
                .find_map(|line| line.trim().strip_prefix(name))
                .and_then(|value| value.trim().trim_end_matches(';').strip_suffix("px"))
                .and_then(|value| value.parse::<u32>().ok())
                .unwrap_or_else(|| panic!(".quick-actions must set {name}"))
        };
        assert_eq!(
            px("margin-right:"),
            22,
            "10px in from the card's edge, plus the card's own 12px margin"
        );
        assert_eq!(px("margin-top:"), 10, "10px down from the card's top");

        let src = include_str!("rows.rs");
        let setup = extract_brace_body(
            src,
            "fn setup(_item: &gtk::ListItem) -> (Self::Root, Self::Widgets) {",
        );
        assert!(
            setup.contains("star_on.set_visible(false)")
                && setup.contains("star_off.set_visible(true)"),
            "the star the plate covers must leave the row entirely while the \
             strip is up -- an `opacity: 0` star still answers the mouse"
        );
        assert!(
            setup.contains("quick_actions.append(&quick_star)")
                && setup.contains("Msg::ToggleStar(id)"),
            "the strip must carry the star it covers, through the same \
             ToggleStar door the row star uses"
        );
    }

    /// T-161: the chip is painted from the value the shell handed down,
    /// and this file has no other source for one. It used to be derived
    /// here from `Thread.labels`, which `Core::map_thread` fills with an
    /// empty vector for every row it maps -- so `display_label` returned
    /// `None` on every real letter and the chip DESIGN.md promises never
    /// appeared. Mutation: read `t.labels` here again -> this test is red.
    #[test]
    fn the_row_chip_comes_from_the_shell_not_from_thread_labels() {
        let src = include_str!("rows.rs");
        let bind = extract_brace_body(src, BIND_THREAD_SIGNATURE);
        assert!(
            bind.contains("if let Some(label) = chip {")
                && bind.contains("w.label_chip.set_label(label);"),
            "the chip is the argument, not something this file works out"
        );
        let live = src.split(&joined(&["mod ", "tests"])).next().unwrap();
        assert!(
            !live.contains(&joined(&["t.la", "bels"])),
            "`Thread.labels` is empty on every row Core maps, so a chip \
             built from it can only ever be invisible"
        );
        assert!(
            live.contains("pub chip: Option<String>,"),
            "the row carries the shell's answer the same way it carries `now`"
        );
    }

    #[test]
    fn setup_bounds_user_controlled_folder_chips_inside_a_row() {
        let src = include_str!("rows.rs");
        let body = extract_brace_body(
            src,
            "fn setup(_item: &gtk::ListItem) -> (Self::Root, Self::Widgets) {",
        );
        for required in [
            "label_chip.set_ellipsize",
            "label_chip.set_max_width_chars(14)",
            "label_chip.set_single_line_mode(true)",
        ] {
            assert!(
                body.contains(required),
                "folder chips must stay within their allocated row width: missing {required}"
            );
        }
    }

    /// Fail-closed on the short context menu: the nine items emit the
    /// same selection-based messages as the toolbar, and the list stays
    /// short -- no Move, no Copy (ТЗ §32).
    #[test]
    fn context_menu_is_the_short_list_of_selection_msgs() {
        let src = include_str!("rows.rs");
        let body = extract_brace_body(
            src,
            "open_id: Rc<RefCell<String>>,\n    popover: &gtk::Popover,\n) -> gtk::Box {",
        );
        for required in [
            "SelectThread",
            "Msg::Reply",
            "Msg::Forward",
            "Msg::MarkRead",
            "Msg::StarSelected",
            "Msg::Archive",
            "Msg::Snooze",
            "Msg::Delete",
            "Msg::PermanentDelete",
        ] {
            assert!(
                body.contains(required),
                "context menu must emit {required} like the toolbar does"
            );
        }
        for forbidden in ["Move", "Copy"] {
            assert!(
                !body.contains(forbidden),
                "context menu stays short: no {forbidden} (ТЗ §32)"
            );
        }
    }

    /// D9/T-003 guard: the list row is a view -- it emits `Msg` and never
    /// names a Core command or a dispatch door. Mutation: call the store
    /// or a command builder from rows.rs -> red.
    #[test]
    fn rows_never_reach_past_the_msg_bus() {
        let src = include_str!("rows.rs");
        for forbidden in [
            joined(&["feathermail_core", "::", "Command"]),
            joined(&["Command", "::"]),
            joined(&[".", "dispatch", "("]),
            joined(&["im", "ap"]),
            joined(&["IM", "AP"]),
        ] {
            assert!(
                !src.contains(&forbidden),
                "rows.rs is a view: it emits Msg and must not contain {forbidden}"
            );
        }
    }

    /// T-099: the live repaint and a fresh bind must be the same write.
    #[test]
    fn a_live_repaint_goes_through_the_factorys_own_bind() {
        let src = include_str!("rows.rs");
        let setup = extract_brace_body(
            src,
            "fn setup(_item: &gtk::ListItem) -> (Self::Root, Self::Widgets) {",
        );
        assert!(
            setup.contains("LIVE_ROWS.with(|rows| rows.borrow_mut().push(widgets.clone()));"),
            "a row can only be repainted in place if its widgets were registered"
        );
        let live = extract_brace_body(
            src,
            "pub fn repaint_live_row(t: &Thread, chip: Option<&str>, preview_pending: bool, now: i64) -> bool {",
        );
        assert!(
            live.contains("bind_thread(w, t, chip, preview_pending, now);"),
            "the in-place repaint must not grow its own copy of bind_thread"
        );
        assert!(
            live.contains("w.thread.is_mapped()"),
            "an unmapped entry is a widget in GTK's recycling pool: it is \
             bound from the model before it is ever shown again"
        );
    }

    /// A row's timestamp and the date header directly above it must come
    /// from one clock. Against the fixture constant every letter newer than
    /// 2024-05-20 fell into `format_clock`'s "Today" branch and printed a
    /// time of day, so a row under an "Older" header read "3:42 PM". The
    /// needle is built by concatenation so this test's own source cannot
    /// satisfy it.
    #[test]
    fn a_list_rows_time_is_measured_against_the_wall_clock() {
        let src = include_str!("rows.rs");
        let live = src.split(&joined(&["mod ", "tests"])).next().unwrap();
        assert!(
            !live.contains(&joined(&["FIXTURE", "_NOW"])),
            "a rendered row's timestamp must be measured against the same \
             wall clock `stamp_headers` uses, not the fixture constant"
        );
        // And the clock reaches the label as a value, so `rows` never has
        // to know where the shell reads it.
        let bind = extract_brace_body(src, BIND_THREAD_SIGNATURE);
        assert!(
            bind.contains("format_clock(t.date, now)"),
            "the row's timestamp is measured against the page's own reading \
             of the clock"
        );
    }
}
