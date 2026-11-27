//! System tray icon (Linux and Windows; macOS has its menu bar item).
//!
//! With a tray icon, closing the window only hides it: RustInput keeps
//! sharing mouse, keyboard and clipboard, and the icon brings the window back
//! or quits. Without a system tray closing the window quits, as before. The
//! menu of the icon also pauses RustInput (the icon then turns grey with a
//! pause sign) and lists the file transfers. A new version of RustInput
//! shows as a green sign with an arrow up on the icon, a line in the tooltip
//! and an item of the menu that gets it.

mod action;
mod icon;
#[cfg(all(unix, not(target_os = "macos")))]
mod linux;
#[cfg(windows)]
mod windows;

#[cfg(all(unix, not(target_os = "macos")))]
use linux as platform;
#[cfg(windows)]
use windows as platform;

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use adw::Application;
use gtk::{glib, glib::clone, prelude::*};
use rustinput_ipc::{FileOptions, FrontendRequest, TransferInfo, TransferState};

pub(crate) use action::{Summary, TrayAction};

use crate::{
    i18n::{tr, trf, trn},
    transfers::Transfers,
    window::Window,
};

/// transfers listed in the menu at most
const MAX_LINES: usize = 6;
/// finished transfers stay in the menu this long, received files waiting to
/// be pasted until they are
const SHOW_FINISHED_MS: u64 = 30_000;

/// the tray icon, removed when the application shuts down
pub(crate) struct Tray {
    handle: RefCell<Option<platform::Handle>>,
    transfers: RefCell<Vec<TransferInfo>>,
    options: Cell<Option<FileOptions>>,
    /// the new version, when there is one
    update: RefCell<Option<String>>,
    /// whether RustInput is paused, once the service said
    paused: Cell<Option<bool>>,
}

impl Tray {
    /// shows the state of the file transfers in the menu and the tooltip
    pub(crate) fn show_transfers(&self, transfers: &[TransferInfo]) {
        self.transfers.replace(transfers.to_vec());
        self.refresh();
    }

    /// shows what happens to copied files
    pub(crate) fn show_options(&self, options: FileOptions) {
        self.options.set(Some(options));
        self.refresh();
    }

    /// offers the new version `latest`, none when there is none
    pub(crate) fn show_update(&self, latest: Option<String>) {
        self.update.replace(latest);
        self.refresh();
    }

    /// shows whether RustInput is paused: the icon, the tooltip, the item
    pub(crate) fn show_paused(&self, paused: bool) {
        self.paused.set(Some(paused));
        self.refresh();
    }

    /// Shows `title` and `text` as a balloon of the icon, which Windows 10
    /// and 11 show as a notification; clicking it shows the window.
    #[cfg(windows)]
    pub(crate) fn notify(&self, title: &str, text: &str) {
        if let Some(handle) = self.handle.borrow().as_ref() {
            handle.notify(title, text);
        }
    }

    fn refresh(&self) {
        if let Some(handle) = self.handle.borrow().as_ref() {
            let now = crate::transfers::now_ms();
            let mut summary = summary(&self.transfers.borrow(), self.options.get(), now);
            if let Some(version) = self.update.borrow().as_ref() {
                let values: [(&str, &dyn std::fmt::Display); 1] = [("version", version)];
                summary.update = Some(trf("Update to RustInput {version}…", &values));
                summary.update_status = Some(crate::notification::title(version));
            }
            summary.paused = self.paused.get();
            handle.show(summary);
        }
    }
}

/// Puts RustInput into the system tray and makes closing `window` hide it.
///
/// Returns `None` if there is no system tray, in which case nothing changes.
pub(crate) fn setup(
    app: &Application,
    window: &Window,
    transfers: &Rc<Transfers>,
) -> Option<Rc<Tray>> {
    let (actions_tx, actions) = async_channel::unbounded();
    // shown in the tooltip and at the top of the menu
    let label = format!(
        "RustInput {}",
        crate::APP_VERSION.get().copied().unwrap_or_default()
    );
    let handle = platform::spawn(actions_tx, label)?;
    log::info!("tray icon shown, closing the window keeps RustInput running");
    let tray = Rc::new(Tray {
        handle: RefCell::new(Some(handle)),
        transfers: Default::default(),
        options: Default::default(),
        update: Default::default(),
        paused: Default::default(),
    });

    // whether the icon is on screen right now
    let available = Rc::new(Cell::new(true));

    window.connect_close_request(clone!(
        #[strong]
        available,
        #[weak]
        app,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |window| {
            if available.get() {
                window.set_visible(false);
                glib::Propagation::Stop
            } else {
                // there would be no way to get the window back
                app.quit();
                glib::Propagation::Proceed
            }
        }
    ));

    // a hidden window must not end the application, and the icon goes away
    // together with it
    let hold = RefCell::new(Some(app.hold()));
    app.connect_shutdown(clone!(
        #[strong]
        tray,
        move |_| {
            tray.handle.borrow_mut().take();
            hold.borrow_mut().take();
        }
    ));

    glib::spawn_future_local(clone!(
        #[weak]
        app,
        #[weak]
        window,
        #[strong]
        transfers,
        async move {
            while let Ok(action) = actions.recv().await {
                match action {
                    TrayAction::Show => window.present(),
                    TrayAction::Quit => app.quit(),
                    TrayAction::CancelTransfers => window.request_cancel_transfers(),
                    TrayAction::ShowTransfers => transfers.present(),
                    TrayAction::ShowClipboards => window.show_clipboards(),
                    TrayAction::Update => window.install_update(),
                    TrayAction::TogglePause => window.request_pause(!window.is_paused()),
                    TrayAction::ToggleSendCopied => {
                        if let Some(options) = window.file_options() {
                            window.request(FrontendRequest::SetFileOptions(FileOptions {
                                send_copied: !options.send_copied,
                                ..options
                            }));
                        }
                    }
                    TrayAction::Available(on_screen) => {
                        available.set(on_screen);
                        if !on_screen && !window.is_visible() {
                            // nothing would be left on screen otherwise
                            window.present();
                        }
                    }
                }
            }
        }
    ));
    Some(tray)
}

/// the item that pauses RustInput or lets it go on
pub(crate) fn pause_label(paused: bool) -> &'static str {
    match paused {
        true => tr("Resume RustInput"),
        false => tr("Pause RustInput"),
    }
}

/// the tooltip line while paused
pub(crate) fn paused_status() -> &'static str {
    tr("Paused: the mouse and keyboard stay on this computer")
}

fn percent(t: &TransferInfo) -> u64 {
    match (t.bytes_done.min(t.bytes) * 100).checked_div(t.bytes) {
        Some(percent) => percent,
        // only empty files
        None if t.files_done >= t.files => 100,
        None => 0,
    }
}

/// "1 file", "3 files"
pub(crate) fn files(n: u32) -> String {
    trn("{count} file", "{count} files", n.into(), &[])
}

fn shorten(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut short: String = text.chars().take(max.saturating_sub(1)).collect();
    short.push('…');
    short
}

fn line(t: &TransferInfo) -> String {
    let arrow = if t.sending { "↑" } else { "↓" };
    let name = &t.peer;
    let files = files(t.files);
    let percent = percent(t);
    let text = match &t.state {
        TransferState::Waiting if t.files == 0 => {
            trf("Preparing files for {name}", &[("name", name)])
        }
        TransferState::Waiting => trf(
            "{files} waiting for {name}",
            &[("files", &files), ("name", name)],
        ),
        TransferState::Running => match &t.current {
            Some(file) => {
                let values: [(&str, &dyn std::fmt::Display); 5] = [
                    ("file", &shorten(file, 40)),
                    ("done", &(t.files_done + 1).min(t.files)),
                    ("total", &t.files),
                    ("name", name),
                    ("percent", &percent),
                ];
                match t.sending {
                    true => trf("{file} ({done}/{total}) to {name}: {percent}%", &values),
                    false => trf("{file} ({done}/{total}) from {name}: {percent}%", &values),
                }
            }
            None => {
                let values: [(&str, &dyn std::fmt::Display); 3] =
                    [("files", &files), ("name", name), ("percent", &percent)];
                match t.sending {
                    true => trf("{files} to {name}: {percent}%", &values),
                    false => trf("{files} from {name}: {percent}%", &values),
                }
            }
        },
        TransferState::Interrupted => {
            let values: [(&str, &dyn std::fmt::Display); 3] =
                [("files", &files), ("name", name), ("percent", &percent)];
            match t.sending {
                true => trf("{files} to {name}: interrupted at {percent}%", &values),
                false => trf("{files} from {name}: interrupted at {percent}%", &values),
            }
        }
        TransferState::Done if t.sending => {
            return format!(
                "✓ {}",
                trf(
                    "{files} sent to {name}",
                    &[("files", &files), ("name", name)]
                )
            );
        }
        TransferState::Done => {
            return format!(
                "✓ {}",
                trf(
                    "{files} from {name}, ready to paste",
                    &[("files", &files), ("name", name)]
                )
            );
        }
        TransferState::Pasted => {
            return format!(
                "✓ {}",
                trf(
                    "{files} from {name}, pasted",
                    &[("files", &files), ("name", name)]
                )
            );
        }
        TransferState::Removed => {
            return format!(
                "✗ {}",
                trf(
                    "{files} from {name}, not pasted",
                    &[("files", &files), ("name", name)]
                )
            );
        }
        TransferState::Failed(reason) => return format!("✗ {name}: {}", shorten(reason, 60)),
    };
    format!("{arrow} {text}")
}

/// what the tray shows for `transfers`: running ones first, then waiting
/// ones, then the most recently finished ones (received files waiting to be
/// pasted, other ones for a while)
pub(crate) fn summary(
    transfers: &[TransferInfo],
    options: Option<FileOptions>,
    now_ms: u64,
) -> Summary {
    let rank = |t: &TransferInfo| match t.state {
        TransferState::Running => 0,
        TransferState::Waiting => 1,
        _ => 2,
    };
    let shown = |t: &&TransferInfo| {
        t.active()
            || (!t.sending && t.state == TransferState::Done)
            || t.finished_ms
                .is_some_and(|f| now_ms.saturating_sub(f) < SHOW_FINISHED_MS)
    };
    let mut sorted: Vec<&TransferInfo> = transfers.iter().filter(shown).collect();
    sorted.sort_by_key(|t| (rank(t), std::cmp::Reverse(t.id)));
    let running: Vec<String> = sorted
        .iter()
        .filter(|t| t.state == TransferState::Running)
        .map(|t| line(t))
        .collect();
    Summary {
        lines: sorted.iter().take(MAX_LINES).map(|t| line(t)).collect(),
        status: (!running.is_empty()).then(|| running.join("\n")),
        cancellable: sorted.iter().any(|t| rank(t) < 2),
        send_copied: options.filter(|o| o.available).map(|o| o.send_copied),
        update: None,
        update_status: None,
        paused: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transfer(id: u64, sending: bool, state: TransferState) -> TransferInfo {
        TransferInfo {
            id,
            sending,
            peer: "windows-pc".into(),
            files: 3,
            files_done: 1,
            bytes: 400,
            bytes_done: 100,
            current: Some("relatório.pdf".into()),
            state,
            started_ms: 0,
            finished_ms: None,
            location: None,
            resumed_bytes: None,
        }
    }

    #[test]
    fn lines_for_every_state() {
        let running = transfer(5, true, TransferState::Running);
        assert_eq!(line(&running), "↑ relatório.pdf (2/3) to windows-pc: 25%");
        let receiving = TransferInfo {
            current: None,
            ..transfer(6, false, TransferState::Running)
        };
        assert_eq!(line(&receiving), "↓ 3 files from windows-pc: 25%");
        assert_eq!(
            line(&transfer(1, false, TransferState::Done)),
            "✓ 3 files from windows-pc, ready to paste"
        );
        assert_eq!(
            line(&TransferInfo {
                files: 1,
                ..transfer(1, true, TransferState::Done)
            }),
            "✓ 1 file sent to windows-pc"
        );
        assert_eq!(
            line(&transfer(
                1,
                true,
                TransferState::Failed("cancelled".into())
            )),
            "✗ windows-pc: cancelled"
        );
        assert_eq!(
            line(&TransferInfo {
                files: 0,
                ..transfer(1, true, TransferState::Waiting)
            }),
            "↑ Preparing files for windows-pc"
        );
    }

    #[test]
    fn running_transfers_come_first() {
        let transfers = [
            transfer(1, false, TransferState::Done),
            transfer(2, true, TransferState::Waiting),
            transfer(3, true, TransferState::Running),
            TransferInfo {
                finished_ms: Some(0),
                ..transfer(4, false, TransferState::Failed("lost".into()))
            },
        ];
        let summary = summary(&transfers, None, 0);
        assert!(summary.lines[0].starts_with("↑ relatório.pdf"));
        assert!(summary.lines[1].contains("waiting"));
        // the most recently finished one first
        assert!(summary.lines[2].starts_with("✗"));
        assert!(summary.lines[3].starts_with("✓"));
        assert!(summary.cancellable);
        assert_eq!(summary.status.as_deref(), Some(summary.lines[0].as_str()));
    }

    #[test]
    fn finished_transfers_can_not_be_cancelled() {
        let done = summary(&[transfer(1, false, TransferState::Done)], None, 0);
        assert!(!done.cancellable);
        assert_eq!(done.status, None);
        assert_eq!(summary(&[], None, 0), Summary::default());
    }

    #[test]
    fn finished_transfers_leave_the_menu() {
        let sent = TransferInfo {
            finished_ms: Some(1_000),
            ..transfer(1, true, TransferState::Done)
        };
        let received = TransferInfo {
            finished_ms: Some(1_000),
            ..transfer(2, false, TransferState::Done)
        };
        let pasted = TransferInfo {
            finished_ms: Some(1_000),
            ..transfer(3, false, TransferState::Pasted)
        };
        let all = [sent, received, pasted];
        assert_eq!(summary(&all, None, 2_000).lines.len(), 3);
        // a minute later only the files waiting to be pasted are left
        let later = summary(&all, None, 61_000);
        assert_eq!(later.lines, ["✓ 3 files from windows-pc, ready to paste"]);
    }

    #[test]
    fn the_option_shows_with_file_transfers_only() {
        let options = FileOptions {
            available: true,
            send_copied: true,
            show_window: true,
            file_manager_menu: true,
            ask_above_mb: 1024,
        };
        assert_eq!(summary(&[], Some(options), 0).send_copied, Some(true));
        let off = FileOptions {
            available: false,
            ..options
        };
        assert_eq!(summary(&[], Some(off), 0).send_copied, None);
        assert_eq!(summary(&[], None, 0).send_copied, None);
    }

    #[test]
    fn long_names_are_shortened() {
        let name = "a".repeat(100);
        let t = TransferInfo {
            current: Some(name),
            ..transfer(1, true, TransferState::Running)
        };
        assert!(line(&t).chars().count() < 80);
    }
}
