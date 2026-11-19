//! Tray icon through the StatusNotifierItem D-Bus interface, shown by KDE,
//! Cinnamon, XFCE, MATE and GNOME with the AppIndicator extension.

use async_channel::Sender;
use ksni::{MenuItem, OfflineReason, ToolTip, blocking::TrayMethods, menu::StandardItem};

use super::{TrayAction, icon};

struct RustInputTray {
    actions: Sender<TrayAction>,
    icons: Vec<ksni::Icon>,
    /// name and version, shown in the tooltip and at the top of the menu
    label: String,
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
        self.icons.clone()
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: self.label.clone(),
            description: "Mouse and keyboard sharing".into(),
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(TrayAction::Show);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            StandardItem {
                label: self.label.clone(),
                enabled: false,
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Show RustInput".into(),
                activate: Box::new(|tray: &mut Self| tray.send(TrayAction::Show)),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit RustInput".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|tray: &mut Self| tray.send(TrayAction::Quit)),
                ..Default::default()
            }
            .into(),
        ]
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

impl Drop for Handle {
    fn drop(&mut self) {
        self.0.shutdown().wait();
    }
}

/// shows the tray icon, `None` if the desktop has no system tray
pub(super) fn spawn(actions: Sender<TrayAction>, label: String) -> Option<Handle> {
    let icons = icon::icons().into_iter().map(argb).collect();
    let tray = RustInputTray {
        actions,
        icons,
        label,
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
