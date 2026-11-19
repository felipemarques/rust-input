//! Tray icon in the Windows notification area.
//!
//! The icon lives on a thread of its own with a plain message loop: tray-icon
//! needs one on the thread that created the icon, and this way it does not
//! depend on how GTK pumps messages on the main thread. Updates of the file
//! transfers reach that thread as a thread message.

use std::{
    sync::{Arc, Mutex, mpsc},
    thread::{self, JoinHandle},
};

use async_channel::Sender;
use tray_icon::{
    Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
};
use windows_sys::Win32::{
    System::Threading::GetCurrentThreadId,
    UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, MSG, PostThreadMessageW, TranslateMessage, WM_APP, WM_QUIT,
    },
};

use super::{Summary, TrayAction, icon};

/// size of the icon handed to Windows, which scales it to the notification
/// area (16 px at 100 %, 32 px at 200 % display scaling)
const ICON_SIZE: u32 = 32;

/// thread message telling the tray thread to show [`Shared::update`]
const WM_TRANSFERS: u32 = WM_APP + 1;

/// tooltips are cut by Windows after 127 characters
const MAX_TOOLTIP: usize = 127;

const SHOW: &str = "show";
const QUIT: &str = "quit";
const CANCEL: &str = "cancel";

/// state handed from GTK to the tray thread
#[derive(Default)]
struct Shared {
    update: Option<Summary>,
}

/// the running tray icon, removed when dropped
pub(super) struct Handle {
    thread_id: u32,
    thread: Option<JoinHandle<()>>,
    shared: Arc<Mutex<Shared>>,
}

impl Handle {
    /// shows the file transfers in the menu and the tooltip
    pub(super) fn show(&self, transfers: Summary) {
        self.shared.lock().expect("lock").update = Some(transfers);
        unsafe {
            PostThreadMessageW(self.thread_id, WM_TRANSFERS, 0, 0);
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // ends the message loop, the icon is removed when the thread drops it
        unsafe {
            PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// shows the tray icon, `None` if it could not be created; `label` (name and
/// version) is shown in the tooltip and at the top of the menu
pub(super) fn spawn(actions: Sender<TrayAction>, label: String) -> Option<Handle> {
    let (ready_tx, ready_rx) = mpsc::channel();
    let shared: Arc<Mutex<Shared>> = Default::default();
    let thread_shared = shared.clone();
    let spawned =
        thread::Builder::new()
            .name("tray".into())
            .spawn(move || match create(actions, &label) {
                Ok(tray) => {
                    let _ = ready_tx.send(Some(unsafe { GetCurrentThreadId() }));
                    run_message_loop(&tray, &label, &thread_shared);
                    drop(tray);
                }
                Err(e) => {
                    log::warn!("could not create the tray icon: {e}");
                    let _ = ready_tx.send(None);
                }
            });
    let thread = match spawned {
        Ok(thread) => thread,
        Err(e) => {
            log::warn!("could not start the tray thread: {e}");
            return None;
        }
    };
    match ready_rx.recv() {
        Ok(Some(thread_id)) => Some(Handle {
            thread_id,
            thread: Some(thread),
            shared,
        }),
        _ => {
            let _ = thread.join();
            None
        }
    }
}

/// the menu of the icon, with the file transfers if there are any
fn menu(label: &str, transfers: &Summary) -> Result<Menu, String> {
    let menu = Menu::new();
    let add = |item: &dyn tray_icon::menu::IsMenuItem| menu.append(item).map_err(|e| e.to_string());
    add(&MenuItem::new(label, false, None))?;
    add(&PredefinedMenuItem::separator())?;
    if !transfers.lines.is_empty() {
        for line in &transfers.lines {
            add(&MenuItem::new(line, false, None))?;
        }
        if transfers.cancellable {
            add(&MenuItem::with_id(CANCEL, "Cancel transfers", true, None))?;
        }
        add(&PredefinedMenuItem::separator())?;
    }
    add(&MenuItem::with_id(SHOW, "Show RustInput", true, None))?;
    add(&PredefinedMenuItem::separator())?;
    add(&MenuItem::with_id(QUIT, "Quit RustInput", true, None))?;
    Ok(menu)
}

fn tooltip(label: &str, transfers: &Summary) -> String {
    let text = match &transfers.status {
        Some(status) => format!("{label}\n{status}"),
        None => label.to_owned(),
    };
    text.chars().take(MAX_TOOLTIP).collect()
}

fn create(actions: Sender<TrayAction>, label: &str) -> Result<TrayIcon, String> {
    let icon = icon::icon(ICON_SIZE).ok_or("no tray icon could be decoded")?;
    let icon = Icon::from_rgba(icon.pixels, icon.width, icon.height).map_err(|e| e.to_string())?;

    // The handlers are global and run on this thread. Only the first call
    // installs one, which is fine as there is a single tray icon.
    let menu_actions = actions.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let action = match event.id.as_ref() {
            SHOW => TrayAction::Show,
            QUIT => TrayAction::Quit,
            CANCEL => TrayAction::CancelTransfers,
            _ => return,
        };
        let _ = menu_actions.try_send(action);
    }));
    TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
        let show = matches!(
            event,
            TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } | TrayIconEvent::DoubleClick {
                button: MouseButton::Left,
                ..
            }
        );
        if show {
            let _ = actions.try_send(TrayAction::Show);
        }
    }));

    let transfers = Summary::default();
    TrayIconBuilder::new()
        .with_icon(icon)
        .with_tooltip(tooltip(label, &transfers))
        .with_menu(Box::new(menu(label, &transfers)?))
        // the left button shows the window, the right one opens the menu
        .with_menu_on_left_click(false)
        .build()
        .map_err(|e| e.to_string())
}

fn run_message_loop(tray: &TrayIcon, label: &str, shared: &Mutex<Shared>) {
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    // GetMessageW returns 0 for WM_QUIT and -1 on errors
    while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {
        if msg.hwnd.is_null() && msg.message == WM_TRANSFERS {
            let update = shared.lock().expect("lock").update.take();
            if let Some(transfers) = update {
                match menu(label, &transfers) {
                    Ok(menu) => tray.set_menu(Some(Box::new(menu))),
                    Err(e) => log::warn!("could not update the tray menu: {e}"),
                }
                if let Err(e) = tray.set_tooltip(Some(tooltip(label, &transfers))) {
                    log::debug!("could not update the tray tooltip: {e}");
                }
            }
            continue;
        }
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
