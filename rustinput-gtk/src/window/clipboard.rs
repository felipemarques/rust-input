//! The Clipboards tab: what this computer and every client have copied, one
//! card each, with "Copy here" to bring it to this computer.
//!
//! While the tab is shown, the service is asked every [`PERIOD`] what
//! changed ([`FrontendRequest::PeekClipboards`]); its answers come as
//! [`Window::clipboard_seen`].
//!
//! Under what a computer holds now comes its history (0.35.0): what it copied
//! before, the latest first, [`FIRST_ITEMS`] of them until "Show all", each
//! with a button that brings it here (on this computer: back on the
//! clipboard).

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::Duration,
};

use adw::prelude::*;
use adw::subclass::prelude::*;
use glib::clone;
use gtk::{gdk, glib};
use rustinput_i18n::{tr, trf, trn};
use rustinput_ipc::{
    ClientHandle, ClipboardHistory, ClipboardHistoryItem, ClipboardPeek, ClipboardSeen,
    FrontendRequest,
};

use super::Window;

/// how often the clipboards are asked about while the tab is shown
const PERIOD: Duration = Duration::from_secs(2);
/// most height of the thumbnail of an image
const THUMBNAIL_HEIGHT: i32 = 180;
/// lines of a text shown before it is cut
const TEXT_LINES: i32 = 8;
/// copies of the history shown until "Show all"
const FIRST_ITEMS: usize = 5;
/// height of the thumbnail of an image in the history
const HISTORY_THUMBNAIL_HEIGHT: i32 = 40;

/// what a button of the history does: bring the copy with this hash
type Bring = Rc<dyn Fn(u64, &gtk::Button)>;

/// the card of one computer
pub struct Card {
    handle: Option<ClientHandle>,
    root: gtk::Box,
    title: gtk::Label,
    summary: gtk::Label,
    body: gtk::Box,
    button: gtk::Button,
    /// what it holds, to bring it here
    hash: Cell<Option<u64>>,
    seen: RefCell<Option<ClipboardSeen>>,
    /// the history: its title, a line when there is none, the copies
    history_box: gtk::Box,
    history_title: gtk::Label,
    history_note: gtk::Label,
    history_list: gtk::ListBox,
    /// "Show all" and back
    more: gtk::Button,
    /// forgets the history, on this computer
    clear: gtk::Button,
    history: RefCell<Option<ClipboardHistory>>,
    show_all: Cell<bool>,
    bring: RefCell<Option<Bring>>,
}

impl Window {
    /// the tab starts asking when shown, stops when hidden
    pub(super) fn setup_clipboards(&self) {
        let imp = self.imp();
        imp.stack.connect_visible_child_name_notify(clone!(
            #[weak(rename_to = window)]
            self,
            move |_| window.update_clipboard_polling()
        ));
        self.connect_visible_notify(|window| window.update_clipboard_polling());
        imp.show_clipboard_switch.connect_active_notify(clone!(
            #[weak(rename_to = window)]
            self,
            move |switch| {
                if !window.imp().updating_show_clipboard.get() {
                    window.request(FrontendRequest::SetShowClipboard(switch.is_active()));
                }
            }
        ));
        imp.keep_history_switch.connect_active_notify(clone!(
            #[weak(rename_to = window)]
            self,
            move |switch| {
                if !window.imp().updating_show_clipboard.get() {
                    window.request(FrontendRequest::SetClipboardHistory(switch.is_active()));
                }
            }
        ));
        // this computer first, before the service answers
        self.card(None, "");
    }

    /// shows the Clipboards tab, from the tray or the radial menu
    pub(crate) fn show_clipboards(&self) {
        self.imp().stack.set_visible_child_name("clipboards");
        self.present();
    }

    fn update_clipboard_polling(&self) {
        let imp = self.imp();
        let shown =
            self.is_visible() && imp.stack.visible_child_name().as_deref() == Some("clipboards");
        let running = imp.clipboard_timer.borrow().is_some();
        if shown && !running {
            self.request(FrontendRequest::PeekClipboards { all: true });
            let timer = glib::timeout_add_local(
                PERIOD,
                clone!(
                    #[weak(rename_to = window)]
                    self,
                    #[upgrade_or]
                    glib::ControlFlow::Break,
                    move || {
                        window.request(FrontendRequest::PeekClipboards { all: false });
                        glib::ControlFlow::Continue
                    }
                ),
            );
            imp.clipboard_timer.replace(Some(timer));
        } else if !shown && running {
            if let Some(timer) = imp.clipboard_timer.take() {
                timer.remove();
            }
        }
    }

    /// whether the other computers see the clipboard of this one, as the
    /// service has it
    pub(crate) fn set_show_clipboard(&self, show: bool) {
        let imp = self.imp();
        imp.updating_show_clipboard.set(true);
        imp.show_clipboard_switch.set_active(show);
        imp.updating_show_clipboard.set(false);
    }

    /// whether this computer keeps a history of its clipboard, as the
    /// service has it
    pub(crate) fn set_keep_clipboard_history(&self, keep: bool) {
        let imp = self.imp();
        imp.updating_show_clipboard.set(true);
        imp.keep_history_switch.set_active(keep);
        imp.updating_show_clipboard.set(false);
    }

    /// the history of the clipboard of a computer, as the service says
    pub(crate) fn clipboard_history(
        &self,
        handle: Option<ClientHandle>,
        history: ClipboardHistory,
    ) {
        let cards = self.imp().clipboard_cards.borrow();
        let Some(card) = cards.iter().find(|c| c.handle == handle) else {
            return;
        };
        card.history.replace(Some(history));
        card.show_history();
    }

    /// what the clipboard of a computer holds, as the service says
    pub(crate) fn clipboard_seen(
        &self,
        handle: Option<ClientHandle>,
        name: &str,
        seen: ClipboardSeen,
    ) {
        self.card(handle, name);
        let cards = self.imp().clipboard_cards.borrow();
        let Some(card) = cards.iter().find(|c| c.handle == handle) else {
            return;
        };
        card.title.set_label(name);
        if card.seen.borrow().as_ref() == Some(&seen) {
            return;
        }
        card.seen.replace(Some(seen.clone()));
        card.show(&seen);
    }

    /// makes the card of the computer, when missing
    fn card(&self, handle: Option<ClientHandle>, name: &str) {
        let imp = self.imp();
        if imp
            .clipboard_cards
            .borrow()
            .iter()
            .any(|c| c.handle == handle)
        {
            return;
        }
        let card = Card::new(handle, name);
        card.button.connect_clicked(clone!(
            #[weak(rename_to = window)]
            self,
            move |button| {
                if let (Some(_), Some(hash)) = (handle, window.card_hash(handle)) {
                    window.bring_clipboard(handle, hash, button);
                }
            }
        ));
        let window = self.downgrade();
        card.bring
            .replace(Some(Rc::new(move |hash, button: &gtk::Button| {
                if let Some(window) = window.upgrade() {
                    window.bring_clipboard(handle, hash, button);
                }
            })));
        card.clear.connect_clicked(clone!(
            #[weak(rename_to = window)]
            self,
            move |_| window.request(FrontendRequest::ClearClipboardHistory)
        ));
        card.more.connect_clicked(clone!(
            #[weak(rename_to = window)]
            self,
            move |_| {
                let cards = window.imp().clipboard_cards.borrow();
                if let Some(card) = cards.iter().find(|c| c.handle == handle) {
                    card.show_all.set(!card.show_all.get());
                    card.show_history();
                }
            }
        ));
        // this computer first, the clients in the order they were made
        let mut cards = imp.clipboard_cards.borrow_mut();
        let at = cards
            .iter()
            .position(|c| match (c.handle, handle) {
                (_, None) => true,
                (None, Some(_)) => false,
                (Some(a), Some(b)) => a > b,
            })
            .unwrap_or(cards.len());
        let after = at.checked_sub(1).map(|i| cards[i].root.clone());
        imp.clipboard_list
            .insert_child_after(&card.root, after.as_ref());
        cards.insert(at, card);
    }

    /// the client was deleted: its card goes
    pub(crate) fn forget_clipboard(&self, handle: ClientHandle) {
        let imp = self.imp();
        let mut cards = imp.clipboard_cards.borrow_mut();
        if let Some(i) = cards.iter().position(|c| c.handle == Some(handle)) {
            imp.clipboard_list.remove(&cards.remove(i).root);
        }
    }

    /// brings the copy with `hash` of the computer here (of this computer:
    /// back on its clipboard)
    fn bring_clipboard(&self, handle: Option<ClientHandle>, hash: u64, button: &gtk::Button) {
        button.set_sensitive(false);
        button.set_label(tr("Copying…"));
        self.request(FrontendRequest::BringClipboard { handle, hash });
    }

    fn card_hash(&self, handle: Option<ClientHandle>) -> Option<u64> {
        let cards = self.imp().clipboard_cards.borrow();
        cards.iter().find(|c| c.handle == handle)?.hash.get()
    }

    /// what the clipboard of `name` held came here, or not (of this
    /// computer, `handle` `None`: back on its clipboard)
    pub(crate) fn clipboard_brought(
        &self,
        handle: Option<ClientHandle>,
        name: &str,
        files: bool,
        error: Option<&str>,
    ) {
        for card in self.imp().clipboard_cards.borrow().iter() {
            card.button.set_sensitive(true);
            card.button.set_label(tr("Copy here"));
            card.show_history();
        }
        match error {
            None if handle.is_none() => self.show_toast(tr("Back on the clipboard")),
            Some(error) if handle.is_none() => self.show_toast(&trf(
                "Could not put it back on the clipboard: {error}",
                &[("error", &error)],
            )),
            None if files => self.show_toast(&trf(
                "The files of {name} are on their way here",
                &[("name", &name)],
            )),
            None => self.show_toast(&trf("Copied here from {name}", &[("name", &name)])),
            Some(error) => self.show_toast(&trf(
                "Could not copy from {name}: {error}",
                &[("name", &name), ("error", &error)],
            )),
        }
        // this computer holds something new
        self.request(FrontendRequest::PeekClipboards { all: false });
    }
}

impl Card {
    fn new(handle: Option<ClientHandle>, name: &str) -> Self {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(10)
            .css_classes(["card", "clipboard-card"])
            .build();
        let header = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(10)
            .build();
        let icon = gtk::Image::from_icon_name(if handle.is_none() {
            "computer-symbolic"
        } else {
            "network-workgroup-symbolic"
        });
        icon.set_pixel_size(24);
        icon.set_valign(gtk::Align::Center);
        let titles = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .hexpand(true)
            .valign(gtk::Align::Center)
            .build();
        let title = gtk::Label::builder()
            .label(name)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["heading"])
            .build();
        let summary = gtk::Label::builder()
            .label(tr("Asking…"))
            .xalign(0.0)
            .wrap(true)
            .css_classes(["dim-label", "caption"])
            .build();
        let title_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .build();
        title_row.append(&title);
        if handle.is_none() {
            let here = gtk::Label::builder()
                .label(tr("This computer"))
                .valign(gtk::Align::Center)
                .css_classes(["clipboard-here", "caption"])
                .build();
            title_row.append(&here);
        }
        titles.append(&title_row);
        titles.append(&summary);
        let button = gtk::Button::builder()
            .label(tr("Copy here"))
            .valign(gtk::Align::Center)
            .tooltip_text(tr("Put it on the clipboard of this computer"))
            .visible(false)
            .css_classes(["suggested-action"])
            .build();
        header.append(&icon);
        header.append(&titles);
        header.append(&button);
        let body = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .visible(false)
            .build();
        root.append(&header);
        root.append(&body);
        let history_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .margin_top(4)
            .visible(false)
            .build();
        let history_header = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .build();
        let history_title = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .css_classes(["heading", "clipboard-history-title"])
            .build();
        let clear = gtk::Button::builder()
            .label(tr("Clear"))
            .tooltip_text(tr("Forget the history of this computer"))
            .valign(gtk::Align::Center)
            .visible(handle.is_none())
            .css_classes(["flat", "caption"])
            .build();
        history_header.append(&history_title);
        history_header.append(&clear);
        let history_note = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .visible(false)
            .css_classes(["dim-label", "caption"])
            .build();
        let history_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        let more = gtk::Button::builder()
            .halign(gtk::Align::Start)
            .visible(false)
            .css_classes(["flat"])
            .build();
        history_box.append(&history_header);
        history_box.append(&history_note);
        history_box.append(&history_list);
        history_box.append(&more);
        root.append(&history_box);
        Self {
            handle,
            root,
            title,
            summary,
            body,
            button,
            hash: Cell::new(None),
            seen: RefCell::new(None),
            history_box,
            history_title,
            history_note,
            history_list,
            more,
            clear,
            history: RefCell::new(None),
            show_all: Cell::new(false),
            bring: RefCell::new(None),
        }
    }

    fn show(&self, seen: &ClipboardSeen) {
        while let Some(child) = self.body.first_child() {
            self.body.remove(&child);
        }
        let (summary, hash) = match seen {
            ClipboardSeen::Holds { hash, peek } => (self.show_peek(peek), Some(*hash)),
            ClipboardSeen::Hidden if self.handle.is_none() => (
                tr("The clipboard is not shared on this computer (clipboard = false)").to_owned(),
                None,
            ),
            ClipboardSeen::Hidden => (tr("Does not show its clipboard").to_owned(), None),
            ClipboardSeen::Unavailable(error) => (
                trf("Could not be asked: {error}", &[("error", error)]),
                None,
            ),
        };
        let holds = matches!(
            seen,
            ClipboardSeen::Holds { peek, .. } if *peek != ClipboardPeek::Empty
        );
        self.hash.set(hash);
        self.summary.set_label(&summary);
        self.body.set_visible(self.body.first_child().is_some());
        self.button.set_visible(holds && self.handle.is_some());
        // what it holds now is not in its history list
        self.show_history();
    }

    /// The history under what it holds now; none while it can not be asked
    /// or does not show its clipboard.
    fn show_history(&self) {
        while let Some(row) = self.history_list.first_child() {
            self.history_list.remove(&row);
        }
        let reachable = matches!(*self.seen.borrow(), Some(ClipboardSeen::Holds { .. }));
        let history = self.history.borrow();
        let (Some(history), true) = (history.as_ref(), reachable) else {
            self.history_box.set_visible(false);
            return;
        };
        let here = self.handle.is_none();
        let note = match history {
            ClipboardHistory::Items(_) => None,
            ClipboardHistory::Off if here => Some(tr(
                "No history is kept on this computer: switch it on in Preferences.",
            )),
            ClipboardHistory::Off => Some(tr("It keeps no history of its clipboard.")),
            ClipboardHistory::Unsupported => Some(tr(
                "New copies can not be noticed on its system yet (Wayland): no history.",
            )),
            ClipboardHistory::Older => Some(tr("Its RustInput is older, without a history.")),
        };
        let current = self.hash.get();
        let items: Vec<&ClipboardHistoryItem> = match history {
            ClipboardHistory::Items(items) => {
                items.iter().filter(|i| Some(i.hash) != current).collect()
            }
            _ => Vec::new(),
        };
        if note.is_none() && items.is_empty() {
            // nothing copied before what it holds now
            self.history_box.set_visible(false);
            return;
        }
        self.history_box.set_visible(true);
        self.history_note.set_visible(note.is_some());
        self.history_note.set_label(note.unwrap_or_default());
        self.history_list.set_visible(!items.is_empty());
        self.clear.set_visible(here && !items.is_empty());
        self.history_title.set_label(&if items.is_empty() {
            tr("History").to_owned()
        } else {
            trn(
                "History · {count} copy before",
                "History · {count} copies before",
                items.len() as u64,
                &[],
            )
        });
        let shown = if self.show_all.get() {
            items.len()
        } else {
            items.len().min(FIRST_ITEMS)
        };
        for item in &items[..shown] {
            self.history_list.append(&self.history_row(item));
        }
        self.more.set_visible(items.len() > FIRST_ITEMS);
        self.more.set_label(&if self.show_all.get() {
            tr("Show fewer").to_owned()
        } else {
            trf("Show all {count}", &[("count", &items.len())])
        });
    }

    /// a copy of the history: what it was, when, and a button that brings it
    fn history_row(&self, item: &ClipboardHistoryItem) -> gtk::ListBoxRow {
        let line = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(10)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(10)
            .margin_end(8)
            .build();
        let (title, detail, icon) = match &item.peek {
            ClipboardPeek::Text {
                start,
                chars,
                formatted,
                ..
            } => {
                let first = start.lines().map(str::trim).find(|l| !l.is_empty());
                let kind = if *formatted {
                    tr("Formatted text")
                } else {
                    tr("Text")
                };
                (
                    first.unwrap_or_default().to_owned(),
                    format!(
                        "{kind} · {}",
                        trn("{count} character", "{count} characters", *chars, &[])
                    ),
                    Some("text-x-generic-symbolic"),
                )
            }
            ClipboardPeek::Image {
                width,
                height,
                thumbnail,
            } => {
                let title = trf(
                    "Image, {width} × {height}",
                    &[("width", width), ("height", height)],
                );
                match texture(thumbnail) {
                    Some(texture) => {
                        let (w, h) = (texture.width().max(1), texture.height().max(1));
                        let height = h.min(HISTORY_THUMBNAIL_HEIGHT);
                        let picture = gtk::Picture::builder()
                            .paintable(&texture)
                            .keep_aspect_ratio(true)
                            .can_shrink(true)
                            .width_request((w * height / h).max(1))
                            .height_request(height)
                            .valign(gtk::Align::Center)
                            .css_classes(["clipboard-history-image"])
                            .build();
                        line.append(&picture);
                        (title, tr("Image").to_owned(), None)
                    }
                    None => (
                        title,
                        tr("Image").to_owned(),
                        Some("image-x-generic-symbolic"),
                    ),
                }
            }
            ClipboardPeek::Files { names, count } => (
                names.join(", "),
                trn(
                    "{count} file or folder",
                    "{count} files and folders",
                    *count,
                    &[],
                ),
                Some("folder-symbolic"),
            ),
            ClipboardPeek::Empty | ClipboardPeek::Secret => {
                (String::new(), String::new(), Some("edit-paste-symbolic"))
            }
        };
        if let Some(icon) = icon {
            let image = gtk::Image::from_icon_name(icon);
            image.set_valign(gtk::Align::Center);
            image.add_css_class("dim-label");
            line.append(&image);
        }
        let titles = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .hexpand(true)
            .valign(gtk::Align::Center)
            .build();
        titles.append(
            &gtk::Label::builder()
                .label(&title)
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .single_line_mode(true)
                .build(),
        );
        titles.append(
            &gtk::Label::builder()
                .label(format!("{} · {detail}", when_label(item.when)))
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .css_classes(["dim-label", "caption"])
                .build(),
        );
        line.append(&titles);
        // copied files of this computer are copied again in the file manager
        let files_here = self.handle.is_none() && matches!(item.peek, ClipboardPeek::Files { .. });
        if !files_here {
            let label = if self.handle.is_none() {
                tr("Copy")
            } else {
                tr("Copy here")
            };
            let button = gtk::Button::builder()
                .label(label)
                .valign(gtk::Align::Center)
                .tooltip_text(tr("Put it on the clipboard of this computer"))
                .build();
            let (bring, hash) = (self.bring.borrow().clone(), item.hash);
            button.connect_clicked(move |button| {
                if let Some(bring) = &bring {
                    bring(hash, button);
                }
            });
            line.append(&button);
        }
        gtk::ListBoxRow::builder()
            .child(&line)
            .activatable(false)
            .build()
    }

    /// fills the body, returns the line under the name
    fn show_peek(&self, peek: &ClipboardPeek) -> String {
        match peek {
            ClipboardPeek::Empty => tr("Empty").to_owned(),
            ClipboardPeek::Text {
                start,
                chars,
                lines,
                formatted,
            } => {
                let text = gtk::Label::builder()
                    .label(start.trim_end())
                    .xalign(0.0)
                    .wrap(true)
                    .wrap_mode(gtk::pango::WrapMode::WordChar)
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .lines(TEXT_LINES)
                    .selectable(true)
                    .css_classes(["clipboard-text", "monospace"])
                    .build();
                self.body.append(&text);
                let kind = if *formatted {
                    tr("Formatted text")
                } else {
                    tr("Text")
                };
                format!(
                    "{kind} · {} · {}",
                    trn("{count} character", "{count} characters", *chars, &[]),
                    trn("{count} line", "{count} lines", *lines, &[])
                )
            }
            ClipboardPeek::Image {
                width,
                height,
                thumbnail,
            } => {
                if let Some(texture) = texture(thumbnail) {
                    // at most THUMBNAIL_HEIGHT high, in its shape
                    let (w, h) = (texture.width().max(1), texture.height().max(1));
                    let height = h.min(THUMBNAIL_HEIGHT);
                    let width = (w * height / h).max(1);
                    let picture = gtk::Picture::builder()
                        .paintable(&texture)
                        .keep_aspect_ratio(true)
                        .can_shrink(true)
                        .width_request(width)
                        .height_request(height)
                        .css_classes(["clipboard-image"])
                        .build();
                    // a row of its own: a picture in a column would take the
                    // height that goes with the width of the whole card
                    let row = gtk::Box::builder()
                        .orientation(gtk::Orientation::Horizontal)
                        .halign(gtk::Align::Start)
                        .build();
                    row.append(&picture);
                    self.body.append(&row);
                }
                trf(
                    "Image, {width} × {height}",
                    &[("width", width), ("height", height)],
                )
            }
            ClipboardPeek::Files { names, count } => {
                let mut list = names.join("\n");
                let more = count.saturating_sub(names.len() as u64);
                if more > 0 {
                    list.push('\n');
                    list.push_str(&trn("and {count} more", "and {count} more", more, &[]));
                }
                let files = gtk::Label::builder()
                    .label(&list)
                    .xalign(0.0)
                    .wrap(true)
                    .wrap_mode(gtk::pango::WrapMode::WordChar)
                    .selectable(true)
                    .css_classes(["clipboard-text"])
                    .build();
                self.body.append(&files);
                trn(
                    "{count} file or folder",
                    "{count} files and folders",
                    *count,
                    &[],
                )
            }
            ClipboardPeek::Secret => {
                let row = gtk::Box::builder()
                    .orientation(gtk::Orientation::Horizontal)
                    .spacing(8)
                    .build();
                let lock = gtk::Image::from_icon_name("dialog-password-symbolic");
                row.append(&lock);
                row.append(
                    &gtk::Label::builder()
                        .label(tr(
                            "A password manager marked it as secret: it is not shown, but it can be copied.",
                        ))
                        .xalign(0.0)
                        .wrap(true)
                        .build(),
                );
                self.body.append(&row);
                tr("Password, hidden").to_owned()
            }
        }
    }
}

/// the time of a copy: the hour today, with the day otherwise
fn when_label(when: u64) -> String {
    let (Ok(then), Ok(now)) = (
        glib::DateTime::from_unix_local(when as i64),
        glib::DateTime::now_local(),
    ) else {
        return String::new();
    };
    let format = if then.ymd() == now.ymd() {
        "%H:%M"
    } else {
        "%e %b, %H:%M"
    };
    then.format(format)
        .map(|s| s.trim().to_owned())
        .unwrap_or_default()
}

/// the thumbnail (a PNG) as something to show; GTK 4.2 can not read it
/// itself (`Texture::from_bytes` came with 4.6)
fn texture(png: &[u8]) -> Option<gdk::Texture> {
    let loader = gtk::gdk_pixbuf::PixbufLoader::new();
    loader.write(png).ok()?;
    loader.close().ok()?;
    #[allow(deprecated)]
    loader
        .pixbuf()
        .map(|pixbuf| gdk::Texture::for_pixbuf(&pixbuf))
}
