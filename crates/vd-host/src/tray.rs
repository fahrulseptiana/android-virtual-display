//! The notification-area icon.
//!
//! A console window is easy to lose behind other windows, and closing it is not
//! how you want to stop a host that clients may be streaming from. The tray icon
//! gives the host a visible "I am running" marker and a deliberate way to quit.
//!
//! Implemented directly on `Shell_NotifyIconW` with a hidden message-only
//! window, which avoids pulling in a windowing toolkit for one icon.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use log::{debug, info, warn};
use windows::core::w;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetCursorPos, GetMessageW, LoadIconW, PostQuitMessage, RegisterClassW,
    SetForegroundWindow, SetTimer, TrackPopupMenu, TranslateMessage, IDI_APPLICATION, MF_STRING,
    TPM_BOTTOMALIGN, TPM_RIGHTBUTTON, WM_APP, WM_COMMAND, WM_DESTROY, WM_LBUTTONUP, WM_RBUTTONUP,
    WM_TIMER, WNDCLASSW,
};

/// Message the shell sends to our window when the icon is interacted with.
const WM_TRAYICON: u32 = WM_APP + 1;
/// Identifies our icon within the window.
const TRAY_ID: u32 = 1;
/// Menu item id for "Exit".
const ID_EXIT: usize = 1001;
/// Timer that watches the shutdown flag.
const TIMER_ID: usize = 1;
/// How often the timer checks whether we should quit.
const TIMER_MS: u32 = 500;

/// The shutdown flag, reachable from the window procedure.
///
/// A window procedure is a plain function pointer, so it cannot capture state;
/// this is the usual workaround.
static RUNNING: OnceLock<Arc<AtomicBool>> = OnceLock::new();

/// Creates the tray icon and pumps messages until the host is asked to stop.
///
/// Blocks the calling thread; the caller keeps the discovery and control
/// services running elsewhere.
pub fn run(running: Arc<AtomicBool>, tooltip: &str) -> Result<(), String> {
    let _ = RUNNING.set(running.clone());

    // SAFETY: every call below is a documented Win32 call; the window and icon
    // live for the duration of the message loop.
    unsafe {
        let module = GetModuleHandleW(None).map_err(|err| format!("GetModuleHandleW: {err}"))?;
        let instance = HINSTANCE(module.0);

        let class_name = w!("VdHostTrayWindow");
        let window_class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: class_name,
            ..Default::default()
        };
        if RegisterClassW(&window_class) == 0 {
            return Err("RegisterClassW failed".to_owned());
        }

        // A message-only window: it has no visual presence of its own.
        let window = CreateWindowExW(
            Default::default(),
            class_name,
            w!("VD Host"),
            Default::default(),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance),
            None,
        )
        .map_err(|err| format!("CreateWindowExW: {err}"))?;

        let icon = LoadIconW(None, IDI_APPLICATION).map_err(|err| format!("LoadIconW: {err}"))?;

        let mut data = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: window,
            uID: TRAY_ID,
            uFlags: NIF_ICON | NIF_TIP | NIF_MESSAGE,
            uCallbackMessage: WM_TRAYICON,
            hIcon: icon,
            ..Default::default()
        };
        set_tooltip(&mut data, tooltip);

        if !Shell_NotifyIconW(NIM_ADD, &data).as_bool() {
            return Err("Shell_NotifyIconW(NIM_ADD) failed".to_owned());
        }
        info!("tray icon added: {tooltip}");

        // Watch the shutdown flag so Ctrl+C and the menu both work.
        SetTimer(Some(window), TIMER_ID, TIMER_MS, None);

        let mut message = Default::default();
        loop {
            let result = GetMessageW(&mut message, None, 0, 0).0;
            match result {
                // WM_QUIT, or an error.
                0 | -1 => break,
                _ => {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
        }

        debug!("tray message loop finished");
    }

    Ok(())
}

/// Handles the window's messages.
unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_TRAYICON => {
            // The low word of lparam is the mouse message that triggered this.
            match (lparam.0 as u32) & 0xffff {
                WM_LBUTTONUP | WM_RBUTTONUP => show_menu(window),
                _ => {}
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            if wparam.0 & 0xffff == ID_EXIT {
                info!("exit requested from the tray");
                request_stop();
                // Destroying the window posts WM_QUIT and leaves the loop.
                let _ = DestroyWindow(window);
            }
            LRESULT(0)
        }
        WM_TIMER => {
            if !is_running() {
                let _ = DestroyWindow(window);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            remove_icon(window);
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(window, message, wparam, lparam),
    }
}

/// Shows the context menu at the cursor.
unsafe fn show_menu(window: HWND) {
    let Ok(menu) = CreatePopupMenu() else {
        warn!("could not create the tray menu");
        return;
    };
    let _ = AppendMenuW(menu, MF_STRING, ID_EXIT, w!("Exit"));

    let mut cursor = POINT::default();
    let _ = GetCursorPos(&mut cursor);

    // Without bringing the window forward first, the menu can stay on screen
    // after the user clicks elsewhere.
    let _ = SetForegroundWindow(window);
    let _ = TrackPopupMenu(
        menu,
        TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
        cursor.x,
        cursor.y,
        None,
        window,
        None,
    );
    let _ = DestroyMenu(menu);
}

/// Removes the icon from the notification area.
unsafe fn remove_icon(window: HWND) {
    let data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: window,
        uID: TRAY_ID,
        ..Default::default()
    };
    let _ = Shell_NotifyIconW(NIM_DELETE, &data);
    debug!("tray icon removed");
}

/// Copies `tooltip` into the fixed-size tip field.
fn set_tooltip(data: &mut NOTIFYICONDATAW, tooltip: &str) {
    let encoded: Vec<u16> = tooltip.encode_utf16().take(data.szTip.len() - 1).collect();
    data.szTip[..encoded.len()].copy_from_slice(&encoded);
}

fn is_running() -> bool {
    RUNNING
        .get()
        .map(|running| running.load(Ordering::Relaxed))
        .unwrap_or(false)
}

fn request_stop() {
    if let Some(running) = RUNNING.get() {
        running.store(false, Ordering::Relaxed);
    }
}
