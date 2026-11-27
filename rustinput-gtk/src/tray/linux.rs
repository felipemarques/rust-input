//! Tray icon through the StatusNotifierItem D-Bus interface, shown by KDE,
//! Cinnamon, XFCE, MATE and GNOME with the AppIndicator extension.

use async_channel::Sender;
use ksni::{
    MenuItem, OfflineReason, ToolTip,
    blocking::TrayMethods,
    menu::{CheckmarkItem, StandardItem},
};

use super::{Summary, TrayAction, away_status, icon, pause_label, paused_status};
use crate::i18n::tr;

struct RustInputTray {
    actions: Sender<TrayAction>,
    /// the icon in every size, in its looks: `[plain, paused, away][new
    /// version]` (grey with a pause sign while paused, a blue sign with an
    /// arrow while the mouse and keyboard are on another computer, a green
    /// sign with an arrow up when a new version is out)
    icons: [[Vec<ksni::Icon>; 2]; 3],
    /// name and version, shown in the tooltip and at the top of the menu
    label: String,
    /// the file transfers
    transfers: Summary,
}

impl RustInputTray {
    fn send(&self, action: TrayAction) {
        // the receiver only goes away when the application quits
        let _ = self.actions.try_send(action);
    }
}

impl ksni::Tray for RustInputTray {
    fn id(&self) -> String {
        "rustinput".into()
    }

    fn title(&self) -> String {
        "RustInput".into()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        let state = match (self.transfers.paused == Some(true), &self.transfers.away) {
            (true, _) => 1,
            (false, Some(_)) => 2,
            (false, None) => 0,
        };
        let new = self.transfers.update.is_some();
        self.icons[state][new as usize].clone()
    }

    fn tool_tip(&self) -> ToolTip {
        let mut description = self
            .transfers
            .status
            .clone()
            .unwrap_or_else(|| tr("Mouse and keyboard sharing").into());
        if self.transfers.paused == Some(true) {
            description = format!("{}\n{description}", paused_status());
        }
        if let Some(name) = &self.transfers.away {
            description = format!("{}\n{description}", away_status(name));
        }
        if let Some(update) = &self.transfers.update_status {
            description = format!("{description}\n{update}");
        }
        ToolTip {
            title: self.label.clone(),
            description,
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(TrayAction::Show);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut menu = vec![
            StandardItem {
                label: self.label.clone(),
                enabled: false,
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
        ];
        if !self.transfers.computers.is_empty() {
            menu.push(
                StandardItem {
                    label: tr("Mouse and keyboard on:").into(),
                    enabled: false,
                    ..Default::default()
                }
                .into(),
            );
            for computer in &self.transfers.computers {
                let handle = computer.handle;
                menu.push(
                    CheckmarkItem {
                        label: computer.name.clone(),
                        checked: computer.here,
                        activate: Box::new(move |tray: &mut Self| {
                            tray.send(TrayAction::GoTo(handle))
                        }),
                        ..Default::default()
                    }
                    .into(),
                );
            }
            menu.push(MenuItem::Separator);
        }
        if let Some(paused) = self.transfers.paused {
            menu.extend([
                StandardItem {
                    label: pause_label(paused).into(),
                    icon_name: if paused {
                        "media-playback-start"
                    } else {
                        "media-playback-pause"
                    }
                    .into(),
                    activate: Box::new(|tray: &mut Self| tray.send(TrayAction::TogglePause)),
                    ..Default::default()
                }
                .into(),
                MenuItem::Separator,
            ]);
        }
        if let Some(update) = &self.transfers.update {
            menu.extend([
                StandardItem {
                    label: update.clone(),
                    icon_name: "software-update-available".into(),
                    activate: Box::new(|tray: &mut Self| tray.send(TrayAction::Update)),
                    ..Default::default()
                }
                .into(),
                MenuItem::Separator,
            ]);
        }
        if !self.transfers.lines.is_empty() {
            for line in &self.transfers.lines {
                menu.push(
                    StandardItem {
                        label: line.clone(),
                        enabled: false,
                        ..Default::default()
                    }
                    .into(),
                );
            }
            if self.transfers.cancellable {
                menu.push(
                    StandardItem {
                        label: tr("Cancel transfers").into(),
                        icon_name: "process-stop".into(),
                        activate: Box::new(|tray: &mut Self| {
                            tray.send(TrayAction::CancelTransfers)
                        }),
                        ..Default::default()
                    }
                    .into(),
                );
            }
            menu.push(MenuItem::Separator);
        }
        if let Some(send_copied) = self.transfers.send_copied {
            menu.extend([
                CheckmarkItem {
                    label: tr("Send copied files automatically").into(),
                    checked: send_copied,
                    activate: Box::new(|tray: &mut Self| tray.send(TrayAction::ToggleSendCopied)),
                    ..Default::default()
                }
                .into(),
                StandardItem {
                    label: tr("File transfers…").into(),
                    icon_name: "folder-download".into(),
                    activate: Box::new(|tray: &mut Self| tray.send(TrayAction::ShowTransfers)),
                    ..Default::default()
                }
                .into(),
                MenuItem::Separator,
            ]);
        }
        menu.extend([
            StandardItem {
                label: tr("Clipboards…").into(),
                icon_name: "edit-paste".into(),
                activate: Box::new(|tray: &mut Self| tray.send(TrayAction::ShowClipboards)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: tr("Show RustInput").into(),
                activate: Box::new(|tray: &mut Self| tray.send(TrayAction::Show)),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: tr("Quit RustInput").into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|tray: &mut Self| tray.send(TrayAction::Quit)),
                ..Default::default()
            }
            .into(),
        ]);
        menu
    }

    fn watcher_online(&self) {
        log::info!("the system tray is back");
        self.send(TrayAction::Available(true));
    }

    fn watcher_offline(&self, reason: OfflineReason) -> bool {
        log::warn!("the system tray went away ({reason:?})");
        self.send(TrayAction::Available(false));
        // keep the service running, the icon returns with the tray
        true
    }
}

/// the running tray icon, removed when dropped
pub(super) struct Handle(ksni::blocking::Handle<RustInputTray>);

impl Handle {
    /// shows the file transfers in the menu and the tooltip
    pub(super) fn show(&self, transfers: Summary) {
        self.0.update(move |tray| tray.transfers = transfers);
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.0.shutdown().wait();
    }
}

/// shows the tray icon, `None` if the desktop has no system tray
pub(super) fn spawn(actions: Sender<TrayAction>, label: String) -> Option<Handle> {
    let originals = icon::icons();
    let looks = |paused, new, away| {
        originals
            .iter()
            .map(|original| argb(icon::look(original, paused, new, away)))
            .collect()
    };
    let icons = [
        [looks(false, false, false), looks(false, true, false)],
        [looks(true, false, false), looks(true, true, false)],
        [looks(false, false, true), looks(false, true, true)],
    ];
    let tray = RustInputTray {
        actions,
        icons,
        label,
        transfers: Summary::default(),
    };
    match tray.spawn() {
        Ok(handle) => Some(Handle(handle)),
        Err(e) => {
            log::info!("no system tray available: {e}");
            None
        }
    }
}

/// StatusNotifierItem icons are ARGB32 in network byte order
fn argb(icon: icon::Rgba) -> ksni::Icon {
    let data = icon
        .pixels
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|&[r, g, b, a]| [a, r, g, b])
        .collect();
    ksni::Icon {
        width: icon.width as i32,
        height: icon.height as i32,
        data,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgba_becomes_argb() {
        let icon = icon::Rgba {
            width: 2,
            height: 1,
            pixels: vec![1, 2, 3, 4, 5, 6, 7, 8],
        };
        let icon = argb(icon);
        assert_eq!((icon.width, icon.height), (2, 1));
        assert_eq!(icon.data, vec![4, 1, 2, 3, 8, 5, 6, 7]);
    }
}
