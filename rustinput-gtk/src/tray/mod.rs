//! System tray icon (Linux and Windows; macOS has its menu bar item).
//!
//! With a tray icon, closing the window only hides it: RustInput keeps
//! sharing mouse, keyboard and clipboard, and the icon brings the window back
//! or quits. Without a system tray closing the window quits, as before. The
//! menu of the icon also lists the file transfers.

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
use rustinput_ipc::{TransferInfo, TransferState};

pub(crate) use action::{Summary, TrayAction};

use crate::window::Window;

/// transfers listed in the menu at most
const MAX_LINES: usize = 6;

/// the tray icon, removed when the application shuts down
pub(crate) struct Tray {
    handle: RefCell<Option<platform::Handle>>,
}

impl Tray {
    /// shows the state of the file transfers in the menu and the tooltip
    pub(crate) fn show_transfers(&self, transfers: &[TransferInfo]) {
        if let Some(handle) = self.handle.borrow().as_ref() {
            handle.show(summary(transfers));
        }
    }
}

/// Puts RustInput into the system tray and makes closing `window` hide it.
///
/// Returns `None` if there is no system tray, in which case nothing changes.
pub(crate) fn setup(app: &Application, window: &Window) -> Option<Rc<Tray>> {
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
        async move {
            while let Ok(action) = actions.recv().await {
                match action {
                    TrayAction::Show => window.present(),
                    TrayAction::Quit => app.quit(),
                    TrayAction::CancelTransfers => window.request_cancel_transfers(),
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
    if n == 1 {
        "1 file".to_owned()
    } else {
        format!("{n} files")
    }
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
    let direction = if t.sending { "to" } else { "from" };
    match &t.state {
        TransferState::Waiting if t.files == 0 => format!("{arrow} Preparing files for {}", t.peer),
        TransferState::Waiting => format!("{arrow} {} waiting for {}", files(t.files), t.peer),
        TransferState::Running => match &t.current {
            Some(name) => format!(
                "{arrow} {} ({}/{}) {direction} {}: {}%",
                shorten(name, 40),
                (t.files_done + 1).min(t.files),
                t.files,
                t.peer,
                percent(t)
            ),
            None => format!(
                "{arrow} {} {direction} {}: {}%",
                files(t.files),
                t.peer,
                percent(t)
            ),
        },
        TransferState::Done if t.sending => format!("✓ {} sent to {}", files(t.files), t.peer),
        TransferState::Done => format!("✓ {} from {}, ready to paste", files(t.files), t.peer),
        TransferState::Failed(reason) => format!("✗ {}: {}", t.peer, shorten(reason, 60)),
    }
}

/// what the tray shows for `transfers`: running ones first, then waiting
/// ones, then the most recently finished ones
pub(crate) fn summary(transfers: &[TransferInfo]) -> Summary {
    let rank = |t: &TransferInfo| match t.state {
        TransferState::Running => 0,
        TransferState::Waiting => 1,
        TransferState::Done | TransferState::Failed(_) => 2,
    };
    let mut sorted: Vec<&TransferInfo> = transfers.iter().collect();
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
            transfer(4, false, TransferState::Failed("lost".into())),
        ];
        let summary = summary(&transfers);
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
        let done = summary(&[transfer(1, false, TransferState::Done)]);
        assert!(!done.cancellable);
        assert_eq!(done.status, None);
        assert_eq!(summary(&[]), Summary::default());
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
