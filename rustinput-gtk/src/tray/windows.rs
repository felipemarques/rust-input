//! Tray icon in the Windows notification area.
//!
//! The icon lives on a thread of its own with a plain message loop: tray-icon
//! needs one on the thread that created the icon, and this way it does not
//! depend on how GTK pumps messages on the main thread.

use std::{
    sync::mpsc,
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
        DispatchMessageW, GetMessageW, MSG, PostThreadMessageW, TranslateMessage, WM_QUIT,
    },
};

use super::{TrayAction, icon};

/// size of the icon handed to Windows, which scales it to the notification
/// area (16 px at 100 %, 32 px at 200 % display scaling)
const ICON_SIZE: u32 = 32;

/// the running tray icon, removed when dropped
pub(super) struct Handle {
    thread_id: u32,
    thread: Option<JoinHandle<()>>,
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
    let spawned =
        thread::Builder::new()
            .name("tray".into())
            .spawn(move || match create(actions, &label) {
                Ok(tray) => {
                    let _ = ready_tx.send(Some(unsafe { GetCurrentThreadId() }));
                    run_message_loop();
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
        }),
        _ => {
            let _ = thread.join();
            None
        }
    }
}

fn create(actions: Sender<TrayAction>, label: &str) -> Result<TrayIcon, String> {
    let icon = icon::icon(ICON_SIZE).ok_or("no tray icon could be decoded")?;
    let icon = Icon::from_rgba(icon.pixels, icon.width, icon.height).map_err(|e| e.to_string())?;

    let header = MenuItem::new(label, false, None);
    let show = MenuItem::new("Show RustInput", true, None);
    let quit = MenuItem::new("Quit RustInput", true, None);
    let menu = Menu::new();
    menu.append_items(&[
        &header,
        &PredefinedMenuItem::separator(),
        &show,
        &PredefinedMenuItem::separator(),
        &quit,
    ])
    .map_err(|e| e.to_string())?;

    // The handlers are global and run on this thread. Only the first call
    // installs one, which is fine as there is a single tray icon.
    let (show_id, quit_id) = (show.id().clone(), quit.id().clone());
    let menu_actions = actions.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let action = if event.id == show_id {
            TrayAction::Show
        } else if event.id == quit_id {
            TrayAction::Quit
        } else {
            return;
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

    TrayIconBuilder::new()
        .with_icon(icon)
        .with_tooltip(label)
        .with_menu(Box::new(menu))
        // the left button shows the window, the right one opens the menu
        .with_menu_on_left_click(false)
        .build()
        .map_err(|e| e.to_string())
}

fn run_message_loop() {
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    // GetMessageW returns 0 for WM_QUIT and -1 on errors
    while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
