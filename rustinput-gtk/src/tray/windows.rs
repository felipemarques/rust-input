//! Tray icon in the Windows notification area.
//!
//! The icon lives on a thread of its own with a plain message loop: tray-icon
//! needs one on the thread that created the icon, and this way it does not
//! depend on how GTK pumps messages on the main thread. Updates of the file
//! transfers reach that thread as a thread message, and so do notifications,
//! shown as a balloon of the icon (Windows 10 and 11 show it as a
//! notification).

use std::{
    sync::{Arc, Mutex, OnceLock, mpsc},
    thread::{self, JoinHandle},
};

use async_channel::Sender;
use tray_icon::{
    Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem},
};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    System::Threading::GetCurrentThreadId,
    UI::{
        Shell::{
            DefSubclassProc, NIF_INFO, NIIF_NONE, NIIF_RESPECT_QUIET_TIME, NIM_MODIFY,
            NIN_BALLOONUSERCLICK, NOTIFYICONDATAW, SetWindowSubclass, Shell_NotifyIconW,
        },
        WindowsAndMessaging::{
            DispatchMessageW, GetMessageW, MSG, PostThreadMessageW, TranslateMessage, WM_APP,
            WM_QUIT,
        },
    },
};

use super::{Summary, TrayAction, away_status, icon, pause_label, paused_status};
use crate::i18n::tr;

/// size of the icon handed to Windows, which scales it to the notification
/// area (16 px at 100 %, 32 px at 200 % display scaling)
const ICON_SIZE: u32 = 32;

/// thread message telling the tray thread to show [`Shared::update`]
const WM_TRANSFERS: u32 = WM_APP + 1;
/// thread message telling the tray thread to show [`Shared::notice`]
const WM_NOTICE: u32 = WM_APP + 2;

/// The message tray-icon has Windows send to the window of the icon about
/// it (`WM_USER_TRAYICON` in tray-icon 0.26), which also tells that a
/// balloon was clicked.
const TRAY_CALLBACK: u32 = 6002;
/// tray-icon numbers its icons from 1 for Windows; one process has one, the
/// first number Windows knows for the window of the icon is it
const MAX_ICON_ID: u32 = 16;
/// the number of the subclass that sees the clicks on balloons
const SUBCLASS: usize = 0x5249_4e42;
/// where a click on a balloon goes
static BALLOON_CLICKS: OnceLock<Sender<TrayAction>> = OnceLock::new();

/// tooltips are cut by Windows after 127 characters
const MAX_TOOLTIP: usize = 127;

const SHOW: &str = "show";
const QUIT: &str = "quit";
const CANCEL: &str = "cancel";
const SEND_COPIED: &str = "send-copied";
const TRANSFERS: &str = "transfers";
const CLIPBOARDS: &str = "clipboards";
const UPDATE: &str = "update";
const PAUSE: &str = "pause";
/// the menu items of the computers: "goto-here", "goto-<handle>"
const GOTO: &str = "goto-";

/// state handed from GTK to the tray thread
#[derive(Default)]
struct Shared {
    update: Option<Summary>,
    /// the title and the text of a notification
    notice: Option<(String, String)>,
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

    /// shows `title` and `text` as a balloon of the icon
    pub(super) fn notify(&self, title: &str, text: &str) {
        self.shared.lock().expect("lock").notice = Some((title.to_owned(), text.to_owned()));
        unsafe {
            PostThreadMessageW(self.thread_id, WM_NOTICE, 0, 0);
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
    if !transfers.computers.is_empty() {
        add(&MenuItem::new(tr("Mouse and keyboard on:"), false, None))?;
        for computer in &transfers.computers {
            let id = match computer.handle {
                Some(handle) => format!("{GOTO}{handle}"),
                None => format!("{GOTO}here"),
            };
            add(&CheckMenuItem::with_id(
                id,
                &computer.name,
                true,
                computer.here,
                None,
            ))?;
        }
        add(&PredefinedMenuItem::separator())?;
    }
    if let Some(paused) = transfers.paused {
        add(&MenuItem::with_id(PAUSE, pause_label(paused), true, None))?;
        add(&PredefinedMenuItem::separator())?;
    }
    if let Some(update) = &transfers.update {
        add(&MenuItem::with_id(UPDATE, update, true, None))?;
        add(&PredefinedMenuItem::separator())?;
    }
    if !transfers.lines.is_empty() {
        for line in &transfers.lines {
            add(&MenuItem::new(line, false, None))?;
        }
        if transfers.cancellable {
            add(&MenuItem::with_id(
                CANCEL,
                tr("Cancel transfers"),
                true,
                None,
            ))?;
        }
        add(&PredefinedMenuItem::separator())?;
    }
    if let Some(send_copied) = transfers.send_copied {
        add(&CheckMenuItem::with_id(
            SEND_COPIED,
            tr("Send copied files automatically"),
            true,
            send_copied,
            None,
        ))?;
        add(&MenuItem::with_id(
            TRANSFERS,
            tr("File transfers…"),
            true,
            None,
        ))?;
        add(&PredefinedMenuItem::separator())?;
    }
    add(&MenuItem::with_id(
        CLIPBOARDS,
        tr("Clipboards…"),
        true,
        None,
    ))?;
    add(&MenuItem::with_id(SHOW, tr("Show RustInput"), true, None))?;
    add(&PredefinedMenuItem::separator())?;
    add(&MenuItem::with_id(QUIT, tr("Quit RustInput"), true, None))?;
    Ok(menu)
}

fn tooltip(label: &str, transfers: &Summary) -> String {
    let mut text = label.to_owned();
    if transfers.paused == Some(true) {
        text = format!("{text}\n{}", paused_status());
    }
    if let Some(name) = &transfers.away {
        text = format!("{text}\n{}", away_status(name));
    }
    if let Some(update) = &transfers.update_status {
        text = format!("{text}\n{update}");
    }
    if let Some(status) = &transfers.status {
        text = format!("{text}\n{status}");
    }
    text.chars().take(MAX_TOOLTIP).collect()
}

/// the icon for the notification area: grey with a pause sign while paused,
/// a blue sign with an arrow while the mouse and keyboard are on another
/// computer, a green sign with an arrow up when a new version is out
fn tray_icon(paused: bool, new: bool, away: bool) -> Result<Icon, String> {
    let icon = icon::icon(ICON_SIZE).ok_or("no tray icon could be decoded")?;
    let icon = icon::look(&icon, paused, new, away);
    Icon::from_rgba(icon.pixels, icon.width, icon.height).map_err(|e| e.to_string())
}

/// `text` into the fixed buffer `to` of Windows, cut so it ends with a 0
fn wide(to: &mut [u16], text: &str) {
    let mut units = text.encode_utf16().take(to.len() - 1).collect::<Vec<_>>();
    // not half of a character at the end
    if units.last().is_some_and(|&u| (0xd800..0xdc00).contains(&u)) {
        units.pop();
    }
    to.fill(0);
    to[..units.len()].copy_from_slice(&units);
}

/// Shows `title` and `text` as a balloon of the icon of the window `hwnd`,
/// which Windows 10 and 11 show as a notification (not during the first
/// hour of a new user, nor while notifications are off). Whether Windows
/// took it.
fn show_balloon(hwnd: HWND, title: &str, text: &str) -> bool {
    let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    data.hWnd = hwnd;
    data.uFlags = NIF_INFO;
    data.dwInfoFlags = NIIF_NONE | NIIF_RESPECT_QUIET_TIME;
    wide(&mut data.szInfoTitle, title);
    wide(&mut data.szInfo, text);
    (1..=MAX_ICON_ID).any(|id| {
        data.uID = id;
        unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) != 0 }
    })
}

/// sees the messages to the window of the icon before tray-icon does: a
/// click on a balloon shows the window
unsafe extern "system" fn balloon_clicks(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    if message == TRAY_CALLBACK && (lparam as u32 & 0xffff) == NIN_BALLOONUSERCLICK {
        if let Some(actions) = BALLOON_CLICKS.get() {
            let _ = actions.try_send(TrayAction::Show);
        }
    }
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}

fn create(actions: Sender<TrayAction>, label: &str) -> Result<TrayIcon, String> {
    let icon = tray_icon(false, false, false)?;
    let _ = BALLOON_CLICKS.set(actions.clone());

    // The handlers are global and run on this thread. Only the first call
    // installs one, which is fine as there is a single tray icon.
    let menu_actions = actions.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let action = match event.id.as_ref() {
            SHOW => TrayAction::Show,
            QUIT => TrayAction::Quit,
            CANCEL => TrayAction::CancelTransfers,
            SEND_COPIED => TrayAction::ToggleSendCopied,
            TRANSFERS => TrayAction::ShowTransfers,
            CLIPBOARDS => TrayAction::ShowClipboards,
            UPDATE => TrayAction::Update,
            PAUSE => TrayAction::TogglePause,
            id => match id.strip_prefix(GOTO) {
                Some("here") => TrayAction::GoTo(None),
                Some(handle) => match handle.parse() {
                    Ok(handle) => TrayAction::GoTo(Some(handle)),
                    Err(_) => return,
                },
                None => return,
            },
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
    let tray = TrayIconBuilder::new()
        .with_icon(icon)
        .with_tooltip(tooltip(label, &transfers))
        .with_menu(Box::new(menu(label, &transfers)?))
        // the left button shows the window, the right one opens the menu
        .with_menu_on_left_click(false)
        .build()
        .map_err(|e| e.to_string())?;
    if unsafe { SetWindowSubclass(tray.window_handle(), Some(balloon_clicks), SUBCLASS, 0) } == 0 {
        log::warn!("clicks on notifications of the tray icon will not show the window");
    }
    Ok(tray)
}

fn run_message_loop(tray: &TrayIcon, label: &str, shared: &Mutex<Shared>) {
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    // whether the icon shows RustInput paused, a new version, and the mouse
    // and keyboard on another computer
    let mut shown = (false, false, false);
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
                let look = (
                    transfers.paused == Some(true),
                    transfers.update.is_some(),
                    transfers.away.is_some(),
                );
                if look != shown {
                    match tray_icon(look.0, look.1, look.2)
                        .and_then(|icon| tray.set_icon(Some(icon)).map_err(|e| e.to_string()))
                    {
                        Ok(()) => shown = look,
                        Err(e) => log::warn!("could not change the tray icon: {e}"),
                    }
                }
            }
            continue;
        }
        if msg.hwnd.is_null() && msg.message == WM_NOTICE {
            let notice = shared.lock().expect("lock").notice.take();
            if let Some((title, text)) = notice {
                match show_balloon(tray.window_handle(), &title, &text) {
                    true => log::info!("notification shown: {title}"),
                    false => log::warn!("Windows did not show the notification: {title}"),
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

#[cfg(test)]
mod tests {
    use windows_sys::{Win32::UI::WindowsAndMessaging::FindWindowW, w};

    use super::*;

    #[test]
    fn texts_are_cut_to_the_buffers_of_windows() {
        let mut title = [1u16; 8];
        wide(&mut title, "RustInput");
        assert_eq!(String::from_utf16_lossy(&title[..7]), "RustInp");
        assert_eq!(title[7], 0);
        // not half of an emoji at the end
        let mut text = [1u16; 4];
        wide(&mut text, "ab😀");
        assert_eq!(text, [b'a' as u16, b'b' as u16, 0, 0]);
    }

    #[test]
    fn the_balloon_reaches_the_icon() {
        // only with a taskbar, which the computers running the tests may
        // not have
        if unsafe { FindWindowW(w!("Shell_TrayWnd"), std::ptr::null()) }.is_null() {
            eprintln!("no taskbar, no notification area to show a balloon in");
            return;
        }
        let icon = tray_icon(false, true).expect("the icon");
        let tray = TrayIconBuilder::new()
            .with_icon(icon)
            .with_tooltip("RustInput test")
            .build()
            .expect("the tray icon");
        assert!(show_balloon(
            tray.window_handle(),
            "RustInput test",
            "A notification of the tests of RustInput"
        ));
    }
}
