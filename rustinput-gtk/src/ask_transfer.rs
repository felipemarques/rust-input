//! Asks whether to receive files another computer sends by themselves: they
//! were copied there when the pointer came here, and are larger than
//! `ask_before_receiving_mb` (see `FrontendEvent::AskTransfer`, 0.38.0).

use std::{
    cell::{Cell, RefCell},
    collections::HashSet,
    rc::Rc,
    time::Duration,
};

use adw::prelude::*;
use gtk::glib;
use rustinput_i18n::{tr, trf, trn};
use rustinput_ipc::{ASK_TRANSFER_SECONDS, FrontendRequest, human_size};

use crate::window::Window;

thread_local! {
    /// the questions shown: a window that connects later has the service
    /// send them all again
    static SHOWN: RefCell<HashSet<u64>> = RefCell::default();
}

/// Shows a small window asking whether to receive `files` files of `bytes`
/// bytes from `peer`; the answer goes to the service as `id`. Without an
/// answer it closes once the service stopped waiting.
pub(crate) fn ask(main: &Window, id: u64, peer: &str, files_count: u32, bytes: u64) {
    if !SHOWN.with(|shown| shown.borrow_mut().insert(id)) {
        return;
    }
    let size = human_size(bytes);
    let title = gtk::Label::builder()
        .label(trf(
            "Receive {size} from {name}?",
            &[("size", &size), ("name", &peer)],
        ))
        .wrap(true)
        .justify(gtk::Justification::Center)
        .build();
    title.add_css_class("title-2");
    let body = gtk::Label::builder()
        .label(trn(
            "{count} file copied on {name} came along with the pointer. It waits in the \
            temporary folder until you paste it.",
            "{count} files copied on {name} came along with the pointer. They wait in the \
            temporary folder until you paste them.",
            files_count.into(),
            &[("name", &peer)],
        ))
        .wrap(true)
        .max_width_chars(44)
        .justify(gtk::Justification::Center)
        .build();
    body.add_css_class("dim-label");
    let refuse = gtk::Button::with_label(tr("Refuse"));
    let receive = gtk::Button::with_label(tr("Receive"));
    receive.add_css_class("suggested-action");
    let buttons = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .halign(gtk::Align::Center)
        .homogeneous(true)
        .margin_top(6)
        .build();
    buttons.append(&refuse);
    buttons.append(&receive);
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(24)
        .margin_bottom(24)
        .margin_start(24)
        .margin_end(24)
        .build();
    content.append(
        &gtk::Image::builder()
            .icon_name("folder-download-symbolic")
            .pixel_size(48)
            .build(),
    );
    content.append(&title);
    content.append(&body);
    content.append(&buttons);
    let outer = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    outer.append(
        &adw::HeaderBar::builder()
            .title_widget(&adw::WindowTitle::new("RustInput", ""))
            .build(),
    );
    outer.append(&content);
    // Not added to the application, like the transfers window: it must not
    // keep RustInput running.
    let window = adw::Window::builder()
        .title(format!("RustInput: {}", tr("Receive files?")))
        .default_width(420)
        .resizable(false)
        .content(&outer)
        .build();

    let answered = Rc::new(Cell::new(false));
    let answer = {
        let (main, window, answered) = (main.downgrade(), window.downgrade(), answered.clone());
        move |receive: Option<bool>| {
            if answered.replace(true) {
                return;
            }
            if let (Some(receive), Some(main)) = (receive, main.upgrade()) {
                main.request(FrontendRequest::AnswerTransfer { id, receive });
            }
            if let Some(window) = window.upgrade() {
                window.close();
            }
        }
    };
    let on_receive = answer.clone();
    receive.connect_clicked(move |_| on_receive(Some(true)));
    let on_refuse = answer.clone();
    refuse.connect_clicked(move |_| on_refuse(Some(false)));
    let on_close = answer.clone();
    window.connect_close_request(move |_| {
        on_close(Some(false));
        glib::Propagation::Proceed
    });
    // the service stops waiting then: the files are not received
    glib::timeout_add_local_once(Duration::from_secs(ASK_TRANSFER_SECONDS), move || {
        answer(None)
    });
    window.present();
    receive.grab_focus();
}
