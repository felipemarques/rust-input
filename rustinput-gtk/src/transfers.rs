//! Window with the progress and history of the file transfers.
//!
//! It opens on its own (unless turned off) when a transfer takes more than a
//! moment, on the sending and on the receiving device, and stays reachable
//! from the tray icon and the main window.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    path::Path,
    rc::Rc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use adw::prelude::*;
use gtk::{gio, glib};
use rustinput_ipc::{FileOptions, FrontendRequest, TransferInfo, TransferState, human_size};

use crate::{
    i18n::{tr, trf, trn},
    window::Window,
};

/// the window opens on its own for transfers running longer than this
const SHOW_AFTER: Duration = Duration::from_secs(1);

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// "1 file", "3 files"
fn files(n: u32) -> String {
    trn("{count} file", "{count} files", n.into(), &[])
}

/// "45 s", "3 min 20 s", "1 h 5 min"
fn duration(seconds: u64) -> String {
    match seconds {
        s if s < 60 => trf("{s} s", &[("s", &s)]),
        s if s < 3600 => trf("{m} min {s} s", &[("m", &(s / 60)), ("s", &(s % 60))]),
        s => trf(
            "{h} h {m} min",
            &[("h", &(s / 3600)), ("m", &((s % 3600) / 60))],
        ),
    }
}

/// what the title of a transfer says
pub(crate) fn title(t: &TransferInfo) -> String {
    let files = files(t.files);
    let values: [(&str, &dyn std::fmt::Display); 2] = [("files", &files), ("name", &t.peer)];
    match (&t.state, t.sending) {
        (TransferState::Waiting, true) if t.files == 0 => {
            trf("Preparing files for {name}", &values)
        }
        (TransferState::Waiting | TransferState::Running, true) => {
            trf("Sending {files} to {name}", &values)
        }
        (TransferState::Waiting | TransferState::Running, false) => {
            trf("Receiving {files} from {name}", &values)
        }
        (TransferState::Done, true) => trf("{files} sent to {name}", &values),
        (TransferState::Done, false) => trf("{files} from {name}: ready to paste", &values),
        (TransferState::Pasted, _) => trf("{files} from {name}: pasted", &values),
        (TransferState::Removed, _) => trf("{files} from {name}: not pasted", &values),
        (TransferState::Failed(_), true) => trf("Sending {files} to {name} failed", &values),
        (TransferState::Failed(_), false) => trf("Receiving {files} from {name} failed", &values),
    }
}

/// the details below the title, `now_ms` for the speed of running ones
pub(crate) fn details(t: &TransferInfo, now_ms: u64) -> String {
    let end = t.finished_ms.unwrap_or(now_ms);
    let seconds = end.saturating_sub(t.started_ms) as f64 / 1000.0;
    let speed = t.bytes_done as f64 / seconds.max(0.001);
    let took = || {
        trf(
            "{size} in {time} ({speed}/s)",
            &[
                ("size", &human_size(t.bytes)),
                ("time", &duration(seconds.round() as u64)),
                ("speed", &human_size(speed as u64)),
            ],
        )
    };
    match &t.state {
        TransferState::Waiting if t.files == 0 => tr("Listing the files…").to_owned(),
        TransferState::Waiting => trf(
            "{size}, waiting for the transfer before it",
            &[("size", &human_size(t.bytes))],
        ),
        TransferState::Running => {
            let mut parts = vec![trf(
                "{done} of {size}",
                &[
                    ("done", &human_size(t.bytes_done)),
                    ("size", &human_size(t.bytes)),
                ],
            )];
            if let Some(current) = &t.current {
                parts.insert(
                    0,
                    format!("{}/{}: {current}", (t.files_done + 1).min(t.files), t.files),
                );
            }
            // the first moments say little about the speed
            if seconds >= 1.0 && speed >= 1.0 {
                parts.push(format!("{}/s", human_size(speed as u64)));
                let left = t.bytes.saturating_sub(t.bytes_done) as f64 / speed;
                parts.push(trf(
                    "{time} left",
                    &[("time", &duration(left.round() as u64))],
                ));
            }
            parts.join(" · ")
        }
        TransferState::Done if t.sending => took(),
        TransferState::Done => trf(
            "{took}. On the clipboard: paste them in a folder to move them there",
            &[("took", &took())],
        ),
        TransferState::Pasted => trf(
            "{size}, moved to where they were pasted. Nothing is left in the temporary folder",
            &[("size", &human_size(t.bytes))],
        ),
        TransferState::Removed => {
            tr("Removed from the temporary folder without being pasted").into()
        }
        TransferState::Failed(reason) => trn(
            "{reason}, after {done} of {count} file",
            "{reason}, after {done} of {count} files",
            t.files.into(),
            &[("reason", reason), ("done", &t.files_done)],
        ),
    }
}

/// how far a transfer got, from 0 to 1
fn fraction(t: &TransferInfo) -> f64 {
    match t.state {
        TransferState::Done | TransferState::Pasted => 1.0,
        _ if t.bytes > 0 => t.bytes_done as f64 / t.bytes as f64,
        _ if t.files > 0 => t.files_done as f64 / t.files as f64,
        _ => 0.0,
    }
}

fn icon(t: &TransferInfo) -> &'static str {
    match (&t.state, t.sending) {
        (TransferState::Failed(_) | TransferState::Removed, _) => "dialog-warning-symbolic",
        (TransferState::Done | TransferState::Pasted, _) => "emblem-ok-symbolic",
        (_, true) => "document-send-symbolic",
        (_, false) => "folder-download-symbolic",
    }
}

/// opens a file or folder with the default application
fn open(path: &Path) {
    let uri = gio::File::for_path(path).uri();
    if let Err(e) = gio::AppInfo::launch_default_for_uri(&uri, None::<&gio::AppLaunchContext>) {
        log::warn!("could not open {path:?}: {e}");
    }
}

struct Row {
    row: gtk::ListBoxRow,
    icon: gtk::Image,
    title: gtk::Label,
    details: gtk::Label,
    bar: gtk::ProgressBar,
    cancel: gtk::Button,
    folder: gtk::Button,
    location: Rc<RefCell<Option<String>>>,
}

pub(crate) struct Transfers {
    window: adw::Window,
    list: gtk::ListBox,
    rows: RefCell<HashMap<u64, Row>>,
    remove: gtk::Button,
    main: glib::WeakRef<Window>,
    /// when running transfers were first seen
    first_seen: RefCell<HashMap<u64, Instant>>,
    /// transfers the window already opened for
    opened_for: RefCell<HashSet<u64>>,
    options: RefCell<Option<FileOptions>>,
}

impl Transfers {
    pub(crate) fn new(main: &Window) -> Rc<Self> {
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .valign(gtk::Align::Start)
            .build();
        list.set_placeholder(Some(
            &gtk::Label::builder()
                .label(
                    "No file transfers yet.\n\nChoose “Copy to RustInput” in the menu of files \
                     and folders in the file manager to send them to the other computer, where \
                     they can be pasted.",
                )
                .wrap(true)
                .justify(gtk::Justification::Center)
                .margin_top(24)
                .margin_bottom(24)
                .margin_start(24)
                .margin_end(24)
                .css_classes(["dim-label"])
                .build(),
        ));
        let scrolled = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(
                &adw::Clamp::builder()
                    .maximum_size(720)
                    .margin_top(12)
                    .margin_bottom(12)
                    .margin_start(12)
                    .margin_end(12)
                    .child(&list)
                    .build(),
            )
            .build();

        let log = gtk::Button::builder()
            .label(tr("Open log"))
            .tooltip_text(tr("Everything sent and received, with every file"))
            .build();
        log.connect_clicked(|_| match rustinput_ipc::transfer_log() {
            Some(path) if path.exists() => open(&path),
            _ => log::info!("no file transfers logged yet"),
        });
        let remove = gtk::Button::builder()
            .label(tr("Remove unpasted files"))
            .tooltip_text(tr(
                "Removes the received files that were not pasted from the temporary folder",
            ))
            .sensitive(false)
            .build();
        let actions = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .margin_top(6)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        actions.append(&log);
        let spacer = gtk::Box::builder().hexpand(true).build();
        actions.append(&spacer);
        actions.append(&remove);

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        content.append(
            &adw::HeaderBar::builder()
                .title_widget(&adw::WindowTitle::new(tr("File transfers"), ""))
                .build(),
        );
        content.append(&scrolled);
        content.append(&actions);

        let version = crate::APP_VERSION.get().copied().unwrap_or_default();
        // Not added to the application: a hidden window of it would keep
        // RustInput running after the main window closed without a tray.
        let window = adw::Window::builder()
            .title(format!("RustInput {version}: {}", tr("File transfers")))
            .default_width(560)
            .default_height(440)
            .hide_on_close(true)
            .content(&content)
            .build();

        let transfers = Rc::new(Self {
            window,
            list,
            rows: Default::default(),
            remove: remove.clone(),
            main: main.downgrade(),
            first_seen: Default::default(),
            opened_for: Default::default(),
            options: Default::default(),
        });
        let weak = Rc::downgrade(&transfers);
        remove.connect_clicked(move |_| {
            if let Some(t) = weak.upgrade() {
                t.request(FrontendRequest::RemoveReceivedFiles);
            }
        });
        transfers
    }

    fn request(&self, request: FrontendRequest) {
        if let Some(main) = self.main.upgrade() {
            main.request(request);
        }
    }

    pub(crate) fn present(&self) {
        self.window.present();
    }

    pub(crate) fn set_options(&self, options: FileOptions) {
        self.options.replace(Some(options));
    }

    /// shows `transfers`, opening the window for one that takes a while
    pub(crate) fn update(self: &Rc<Self>, transfers: &[TransferInfo]) {
        let now = now_ms();
        let ids: HashSet<u64> = transfers.iter().map(|t| t.id).collect();
        self.rows.borrow_mut().retain(|id, row| {
            let keep = ids.contains(id);
            if !keep {
                self.list.remove(&row.row);
            }
            keep
        });
        // newest first: ids grow with time
        let mut sorted: Vec<&TransferInfo> = transfers.iter().collect();
        sorted.sort_by_key(|t| t.id);
        for t in sorted {
            if !self.rows.borrow().contains_key(&t.id) {
                let row = self.row(t.id);
                self.list.prepend(&row.row);
                self.rows.borrow_mut().insert(t.id, row);
            }
            if let Some(row) = self.rows.borrow().get(&t.id) {
                row.icon.set_icon_name(Some(icon(t)));
                row.title.set_label(&title(t));
                row.details.set_label(&details(t, now));
                row.bar.set_fraction(fraction(t));
                row.bar.set_visible(t.state != TransferState::Removed);
                row.cancel.set_visible(t.active());
                row.folder.set_visible(t.location.is_some());
                row.location.replace(t.location.clone());
            }
        }
        self.remove.set_sensitive(
            transfers
                .iter()
                .any(|t| !t.sending && t.state == TransferState::Done),
        );
        self.open_for_long_transfers(transfers);
    }

    fn open_for_long_transfers(&self, transfers: &[TransferInfo]) {
        let mut first_seen = self.first_seen.borrow_mut();
        first_seen.retain(|id, _| transfers.iter().any(|t| t.id == *id && t.active()));
        let show = self.options.borrow().is_some_and(|o| o.show_window);
        for t in transfers.iter().filter(|t| t.active()) {
            let seen = *first_seen.entry(t.id).or_insert_with(Instant::now);
            if show && seen.elapsed() >= SHOW_AFTER && self.opened_for.borrow_mut().insert(t.id) {
                // shown without asking for the focus: the user may be typing
                // somewhere else
                if !self.window.is_visible() {
                    self.window.set_visible(true);
                }
            }
        }
    }

    fn row(self: &Rc<Self>, id: u64) -> Row {
        let icon = gtk::Image::builder()
            .pixel_size(24)
            .valign(gtk::Align::Center)
            .build();
        let title = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["heading"])
            .build();
        let details = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["dim-label"])
            .build();
        let bar = gtk::ProgressBar::builder().margin_top(4).build();
        let text = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .hexpand(true)
            .build();
        text.append(&title);
        text.append(&details);
        text.append(&bar);
        let cancel = gtk::Button::builder()
            .icon_name("process-stop-symbolic")
            .tooltip_text(tr("Cancel"))
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        let folder = gtk::Button::builder()
            .icon_name("folder-open-symbolic")
            .tooltip_text(tr("Open the temporary folder with the received files"))
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        let location: Rc<RefCell<Option<String>>> = Default::default();
        let weak = Rc::downgrade(self);
        cancel.connect_clicked(move |_| {
            if let Some(t) = weak.upgrade() {
                t.request(FrontendRequest::CancelTransfer(id));
            }
        });
        let folder_location = location.clone();
        folder.connect_clicked(move |_| {
            if let Some(location) = folder_location.borrow().as_ref() {
                open(Path::new(location));
            }
        });
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .margin_top(10)
            .margin_bottom(10)
            .margin_start(12)
            .margin_end(8)
            .build();
        content.append(&icon);
        content.append(&text);
        content.append(&folder);
        content.append(&cancel);
        let row = gtk::ListBoxRow::builder()
            .activatable(false)
            .child(&content)
            .build();
        Row {
            row,
            icon,
            title,
            details,
            bar,
            cancel,
            folder,
            location,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transfer(sending: bool, state: TransferState) -> TransferInfo {
        TransferInfo {
            id: 1,
            sending,
            peer: "windows-pc".into(),
            files: 37,
            files_done: 9,
            bytes: 100 * 1024 * 1024,
            bytes_done: 25 * 1024 * 1024,
            current: Some("relatório.pdf".into()),
            state,
            started_ms: 10_000,
            finished_ms: None,
            location: None,
        }
    }

    #[test]
    fn running_transfers_show_speed_and_time_left() {
        let t = transfer(false, TransferState::Running);
        assert_eq!(title(&t), "Receiving 37 files from windows-pc");
        // 25 MB in 5 s: 5 MB/s, 75 MB to go
        assert_eq!(
            details(&t, 15_000),
            "10/37: relatório.pdf · 25.0 MB of 100.0 MB · 5.0 MB/s · 15 s left"
        );
        assert!((fraction(&t) - 0.25).abs() < 1e-9);
    }

    #[test]
    fn finished_transfers() {
        let sent = TransferInfo {
            bytes_done: 100 * 1024 * 1024,
            finished_ms: Some(30_000),
            ..transfer(true, TransferState::Done)
        };
        assert_eq!(title(&sent), "37 files sent to windows-pc");
        assert_eq!(details(&sent, 0), "100.0 MB in 20 s (5.0 MB/s)");
        let pasted = transfer(false, TransferState::Pasted);
        assert_eq!(title(&pasted), "37 files from windows-pc: pasted");
        assert_eq!(fraction(&pasted), 1.0);
        let failed = transfer(true, TransferState::Failed("cancelled".into()));
        assert_eq!(details(&failed, 0), "cancelled, after 9 of 37 files");
    }

    #[test]
    fn durations() {
        assert_eq!(duration(45), "45 s");
        assert_eq!(duration(200), "3 min 20 s");
        assert_eq!(duration(3900), "1 h 5 min");
    }
}
