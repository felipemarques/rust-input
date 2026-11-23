//! The Clipboards tab: what this computer and every client have copied, one
//! card each, with "Copy here" to bring it to this computer.
//!
//! While the tab is shown, the service is asked every [`PERIOD`] what
//! changed ([`FrontendRequest::PeekClipboards`]); its answers come as
//! [`Window::clipboard_seen`].

use std::{
    cell::{Cell, RefCell},
    time::Duration,
};

use adw::prelude::*;
use adw::subclass::prelude::*;
use glib::clone;
use gtk::{gdk, glib};
use rustinput_i18n::{tr, trf, trn};
use rustinput_ipc::{ClientHandle, ClipboardPeek, ClipboardSeen, FrontendRequest};

use super::Window;

/// how often the clipboards are asked about while the tab is shown
const PERIOD: Duration = Duration::from_secs(2);
/// most height of the thumbnail of an image
const THUMBNAIL_HEIGHT: i32 = 180;
/// lines of a text shown before it is cut
const TEXT_LINES: i32 = 8;

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
        // this computer first, before the service answers
        self.card(None, "");
    }

    /// shows the Clipboards tab, from the tray (macOS has its own menu bar
    /// item, without it)
    #[cfg_attr(target_os = "macos", allow(dead_code))]
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
        card.show(&seen);
        card.seen.replace(Some(seen));
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
            move |button| window.bring_clipboard(handle, button)
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

    fn bring_clipboard(&self, handle: Option<ClientHandle>, button: &gtk::Button) {
        let (Some(handle), Some(hash)) = (handle, self.card_hash(handle)) else {
            return;
        };
        button.set_sensitive(false);
        button.set_label(tr("Copying…"));
        self.request(FrontendRequest::BringClipboard { handle, hash });
    }

    fn card_hash(&self, handle: Option<ClientHandle>) -> Option<u64> {
        let cards = self.imp().clipboard_cards.borrow();
        cards.iter().find(|c| c.handle == handle)?.hash.get()
    }

    /// what the clipboard of `name` held came here, or not
    pub(crate) fn clipboard_brought(&self, name: &str, files: bool, error: Option<&str>) {
        for card in self.imp().clipboard_cards.borrow().iter() {
            card.button.set_sensitive(true);
            card.button.set_label(tr("Copy here"));
        }
        match error {
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
        Self {
            handle,
            root,
            title,
            summary,
            body,
            button,
            hash: Cell::new(None),
            seen: RefCell::new(None),
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
