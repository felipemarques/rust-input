//! Asks whether to authorize a computer an authorized one was paired with
//! (`FrontendEvent::Introduction`, 0.41.0): nothing is authorized on the word
//! of another computer alone.

use std::{
    cell::{Cell, RefCell},
    collections::HashSet,
    rc::Rc,
};

use adw::prelude::*;
use gtk::glib;
use rustinput_i18n::{tr, trf};
use rustinput_ipc::FrontendRequest;

use crate::window::Window;

thread_local! {
    /// the fingerprints asked about: a window that connects later has the
    /// service send them all again
    static SHOWN: RefCell<HashSet<String>> = RefCell::default();
}

/// the fingerprint in groups of four pairs, on two lines
fn grouped(fingerprint: &str) -> String {
    let pairs: Vec<&str> = fingerprint.split(':').collect();
    let groups: Vec<String> = pairs.chunks(4).map(|c| c.join(":")).collect();
    let half = groups.len().div_ceil(2);
    format!(
        "{}\n{}",
        groups[..half].join("  "),
        groups[half..].join("  ")
    )
}

/// Shows a small window asking whether to authorize `name` (`fingerprint`),
/// which `by` was paired with.
pub(crate) fn ask(main: &Window, by: &str, name: &str, fingerprint: &str) {
    if !SHOWN.with(|shown| shown.borrow_mut().insert(fingerprint.to_owned())) {
        return;
    }
    let title = gtk::Label::builder()
        .label(trf("Authorize {name} here too?", &[("name", &name)]))
        .wrap(true)
        .justify(gtk::Justification::Center)
        .build();
    title.add_css_class("title-2");
    let body = gtk::Label::builder()
        .label(trf(
            "{by} was paired with {name}. Authorized here too, the two can control each other and \
            share the clipboard and files. Compare the fingerprint with the one {name} shows.",
            &[("by", &by), ("name", &name)],
        ))
        .wrap(true)
        .max_width_chars(46)
        .justify(gtk::Justification::Center)
        .build();
    body.add_css_class("dim-label");
    let print = gtk::Label::builder()
        .label(grouped(fingerprint))
        .selectable(true)
        .justify(gtk::Justification::Center)
        .build();
    print.add_css_class("monospace");
    let later = gtk::Button::with_label(tr("Not now"));
    let authorize = gtk::Button::with_label(tr("Authorize"));
    authorize.add_css_class("suggested-action");
    let buttons = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .halign(gtk::Align::Center)
        .homogeneous(true)
        .margin_top(6)
        .build();
    buttons.append(&later);
    buttons.append(&authorize);
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
            .icon_name("system-users-symbolic")
            .pixel_size(48)
            .build(),
    );
    content.append(&title);
    content.append(&body);
    content.append(&print);
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
    // not added to the application: it must not keep RustInput running
    let window = adw::Window::builder()
        .title(format!("RustInput: {}", tr("Authorize a computer?")))
        .default_width(440)
        .resizable(false)
        .content(&outer)
        .build();
    let answered = Rc::new(Cell::new(false));
    let fingerprint = fingerprint.to_owned();
    let answer = {
        let (main, window, answered) = (main.downgrade(), window.downgrade(), answered.clone());
        move |authorize: bool| {
            if answered.replace(true) {
                return;
            }
            if let Some(main) = main.upgrade() {
                main.request(FrontendRequest::AnswerIntroduction {
                    fingerprint: fingerprint.clone(),
                    authorize,
                });
            }
            if let Some(window) = window.upgrade() {
                window.close();
            }
        }
    };
    let on_authorize = answer.clone();
    authorize.connect_clicked(move |_| on_authorize(true));
    let on_later = answer.clone();
    later.connect_clicked(move |_| on_later(false));
    window.connect_close_request(move |_| {
        answer(false);
        glib::Propagation::Proceed
    });
    window.present();
    // not on the fingerprint (it would be selected) nor on authorizing
    later.grab_focus();
}
