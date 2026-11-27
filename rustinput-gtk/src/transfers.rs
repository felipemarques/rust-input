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
use rustinput_ipc::{
    FileOptions, FrontendRequest, LEFT_OUT_CHANGED, LEFT_OUT_GONE, LEFT_OUT_LINK, LEFT_OUT_SPECIAL,
    LEFT_OUT_UNREADABLE, LeftOut, TransferInfo, TransferState, human_size, visible_name,
};

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
        (TransferState::Interrupted, true) => {
            trf("Sending {files} to {name}: interrupted", &values)
        }
        (TransferState::Interrupted, false) => {
            trf("Receiving {files} from {name}: interrupted", &values)
        }
    }
}

/// the details below the title, `now_ms` for the speed of running ones
pub(crate) fn details(t: &TransferInfo, now_ms: u64) -> String {
    let end = t.finished_ms.unwrap_or(now_ms);
    let seconds = end.saturating_sub(t.started_ms) as f64 / 1000.0;
    // went on where it stopped: since then, and what came since then
    let resumed = t.resumed_bytes.unwrap_or(0);
    let speed = t.bytes_done.saturating_sub(resumed) as f64 / seconds.max(0.001);
    let took = || {
        let values: [(&str, &dyn std::fmt::Display); 3] = [
            ("size", &human_size(t.bytes)),
            ("time", &duration(seconds.round() as u64)),
            ("speed", &human_size(speed as u64)),
        ];
        match t.resumed_bytes {
            Some(at) => trf(
                "{size}, the last {rest} in {time} ({speed}/s): it went on where it had stopped",
                &[
                    ("size", &human_size(t.bytes)),
                    ("rest", &human_size(t.bytes.saturating_sub(at))),
                    ("time", &duration(seconds.round() as u64)),
                    ("speed", &human_size(speed as u64)),
                ],
            ),
            None => trf("{size} in {time} ({speed}/s)", &values),
        }
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
                    format!(
                        "{}/{}: {}",
                        (t.files_done + 1).min(t.files),
                        t.files,
                        visible_name(current)
                    ),
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
        TransferState::Interrupted => trf(
            "{done} of {size}. The connection was lost: it goes on from here when {name} is back",
            &[
                ("done", &human_size(t.bytes_done)),
                ("size", &human_size(t.bytes)),
                ("name", &t.peer),
            ],
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

/// why something did not go or come as it was
fn why(item: &LeftOut) -> String {
    match item.why.as_str() {
        LEFT_OUT_LINK => tr("a link to a folder").to_owned(),
        LEFT_OUT_UNREADABLE => trf("could not be read: {error}", &[("error", &item.detail)]),
        LEFT_OUT_SPECIAL => tr("not a file or a folder").to_owned(),
        LEFT_OUT_CHANGED => tr("changed while it was sent").to_owned(),
        LEFT_OUT_GONE => tr("removed before it was sent").to_owned(),
        // a newer RustInput
        _ if item.detail.is_empty() => item.why.clone(),
        _ => item.detail.clone(),
    }
}

/// the name of a path of a transfer, without its folders
fn name_of(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// What of a transfer did not go or come as it was: a short line, and all
/// of it for the tooltip. Empty when everything did.
pub(crate) fn left_out(t: &TransferInfo) -> (String, String) {
    const SHOWN: usize = 3;
    let mut short = Vec::new();
    let mut long = Vec::new();
    let (renamed, missing): (Vec<&LeftOut>, Vec<&LeftOut>) =
        t.left_out.iter().partition(|i| i.came());
    let list = |items: &[&LeftOut], each: &dyn Fn(&LeftOut) -> String| {
        let mut text: Vec<String> = items.iter().take(SHOWN).map(|i| each(i)).collect();
        if items.len() > SHOWN {
            text.push(trf("{count} more", &[("count", &(items.len() - SHOWN))]));
        }
        text.join(", ")
    };
    let missed = |i: &LeftOut| format!("{} ({})", visible_name(name_of(&i.path)), why(i));
    let moved = |i: &LeftOut| {
        format!(
            "{} → {}",
            visible_name(name_of(&i.path)),
            visible_name(&i.detail)
        )
    };
    if !missing.is_empty() {
        let values: [(&str, &dyn std::fmt::Display); 1] = [("list", &list(&missing, &missed))];
        short.push(match t.sending {
            true => trf("Not sent: {list}", &values),
            false => trf("Did not come: {list}", &values),
        });
        long.extend(
            missing
                .iter()
                .map(|i| format!("{} ({})", visible_name(&i.path), why(i))),
        );
    }
    if !renamed.is_empty() {
        let values: [(&str, &dyn std::fmt::Display); 1] = [("list", &list(&renamed, &moved))];
        short.push(trf(
            "Renamed, the name is not allowed here: {list}",
            &values,
        ));
        long.extend(
            renamed
                .iter()
                .map(|i| format!("{} → {}", visible_name(&i.path), visible_name(&i.detail))),
        );
    }
    (short.join(". "), long.join("\n"))
}

struct Row {
    row: gtk::ListBoxRow,
    icon: gtk::Image,
    title: gtk::Label,
    details: gtk::Label,
    /// what did not go or come as it was
    notes: gtk::Label,
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
                let (notes, all) = left_out(t);
                row.notes.set_label(&notes);
                row.notes.set_tooltip_text(Some(&all));
                row.notes.set_visible(!notes.is_empty());
                row.bar.set_fraction(fraction(t));
                row.bar.set_visible(t.state != TransferState::Removed);
                row.cancel.set_visible(t.active());
                row.folder.set_visible(t.location.is_some());
                row.location.replace(t.location.clone());
            }
        }
        // what arrived of interrupted transfers goes too
        self.remove.set_sensitive(transfers.iter().any(|t| {
            !t.sending && matches!(t.state, TransferState::Done | TransferState::Interrupted)
        }));
        self.open_for_long_transfers(transfers);
    }

    fn open_for_long_transfers(&self, transfers: &[TransferInfo]) {
        let mut first_seen = self.first_seen.borrow_mut();
        first_seen.retain(|id, _| transfers.iter().any(|t| t.id == *id && t.active()));
        let show = self.options.borrow().is_some_and(|o| o.show_window);
        // not for one waiting for its connection
        let moving = |t: &&TransferInfo| t.active() && t.state != TransferState::Interrupted;
        for t in transfers.iter().filter(moving) {
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
        let notes = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .visible(false)
            .css_classes(["warning", "caption"])
            .build();
        let bar = gtk::ProgressBar::builder().margin_top(4).build();
        let text = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .hexpand(true)
            .build();
        text.append(&title);
        text.append(&details);
        text.append(&notes);
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
            notes,
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
    use rustinput_ipc::LEFT_OUT_RENAMED;

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
            resumed_bytes: None,
            left_out: Vec::new(),
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

    #[test]
    fn what_did_not_come_is_told() {
        let mut t = transfer(false, TransferState::Done);
        assert_eq!(left_out(&t), (String::new(), String::new()));
        t.left_out = vec![
            LeftOut::new("Pasta/volta", LEFT_OUT_LINK, ""),
            LeftOut::new("Pasta/a.txt", LEFT_OUT_CHANGED, ""),
            LeftOut::new("Pasta/b.txt", LEFT_OUT_GONE, ""),
            LeftOut::new("Pasta/c.txt", LEFT_OUT_UNREADABLE, "Permission denied"),
            LeftOut::new("Pasta/x:y.txt", LEFT_OUT_RENAMED, "x_y.txt"),
            LeftOut::new("Pasta/foto\u{202e}gpj.exe", LEFT_OUT_RENAMED, "fotogpj.exe"),
        ];
        let (short, all) = left_out(&t);
        assert_eq!(
            short,
            "Did not come: volta (a link to a folder), a.txt (changed while it was sent), \
            b.txt (removed before it was sent), 1 more. \
            Renamed, the name is not allowed here: x:y.txt → x_y.txt, \
            foto⟨U+202E⟩gpj.exe → fotogpj.exe"
        );
        assert_eq!(all.lines().count(), 6);
        assert!(all.contains("Pasta/c.txt (could not be read: Permission denied)"));
        t.sending = true;
        assert!(left_out(&t).0.starts_with("Not sent: volta"));
    }
}
