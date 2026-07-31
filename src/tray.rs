//! System tray icon — Shell_NotifyIconW wrapper, popup menu,
//! hidden notification window, and independent message pump thread.
//! Modeled after the mouse-gesture project's proven pattern.

use crate::win32::*;
use anyhow::Result;
use crossbeam::channel::{Receiver, Sender};
use std::thread;
use tracing::{error, info, warn};

/// Commands from the tray icon back to the main application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayCommand {
    /// User clicked "Open" or left-clicked the tray icon — restore the window.
    Open,
    /// User clicked "Exit" — quit the application.
    Exit,
}

// Menu item IDs
const IDM_OPEN: u32 = 1001;
const IDM_EXIT: u32 = 1002;

/// Convert a Rust &str to a null-terminated UTF-16 wide string.
fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Spawn the tray icon in a background thread with its own message pump.
/// Returns a receiver for tray commands.
pub fn spawn(icon_path: &str, tooltip: &str) -> Result<Receiver<TrayCommand>> {
    let wide_path = to_wide(icon_path);

    // Load the icon
    let hicon = unsafe {
        LoadImageW(
            0,
            PCWSTR(wide_path.as_ptr()),
            IMAGE_ICON,
            0,
            0,
            LR_LOADFROMFILE | LR_DEFAULTSIZE,
        )
    };

    let hicon = if !hicon.is_null() {
        info!("Tray icon loaded from {}", icon_path);
        hicon as isize
    } else {
        warn!("Failed to load tray icon from {}, using default", icon_path);
        0isize
    };

    let (tx, rx) = crossbeam::channel::bounded::<TrayCommand>(8);
    let tooltip = tooltip.to_string();

    thread::Builder::new().name("tray".into()).spawn(move || {
        if let Err(e) = tray_thread(&tooltip, hicon, tx) {
            error!("Tray thread error: {}", e);
        }
    })?;

    Ok(rx)
}

/// The tray icon thread body.
fn tray_thread(tooltip: &str, hicon: isize, tx: Sender<TrayCommand>) -> Result<()> {
    let class_name = to_wide("PapervaultTray");

    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(tray_window_proc),
        hInstance: unsafe { GetModuleHandleW(std::ptr::null()) as isize },
        lpszClassName: class_name.as_ptr(),
        ..Default::default()
    };

    unsafe {
        let atom = RegisterClassExW(&wc);
        if atom == 0 {
            anyhow::bail!("RegisterClassExW failed");
        }
    }

    let window_name = to_wide("PapervaultTray");
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class_name.as_ptr()),
            PCWSTR(window_name.as_ptr()),
            WS_OVERLAPPED,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            0,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
        )
    };

    if hwnd == 0 {
        anyhow::bail!("CreateWindowExW failed");
    }

    // Store the sender in GWLP_USERDATA so the window proc can access it
    let tx_ptr = Box::into_raw(Box::new(tx));
    unsafe {
        SetWindowLongPtrW(hwnd, GWL_USERDATA, tx_ptr as isize);
    }

    // Create the tray icon
    let uid = 1u32;
    let callback_msg = WM_APP + 1;

    let tip = to_wide(&format!("{}\0", tooltip));

    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: uid,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: callback_msg,
        hIcon: hicon,
        ..Default::default()
    };

    let tip_slice = &mut nid.szTip;
    let copy_len = tip.len().min(tip_slice.len());
    tip_slice[..copy_len].copy_from_slice(&tip[..copy_len]);

    unsafe {
        let ok = Shell_NotifyIconW(NIM_ADD, &nid);
        if ok == 0 {
            anyhow::bail!("Shell_NotifyIconW(NIM_ADD) failed");
        }
    }
    info!("Tray icon added");

    // Message pump
    let mut msg = MSG::default();
    loop {
        unsafe {
            let ret = GetMessageW(&mut msg, 0, 0, 0);
            if ret == 0 || ret == -1 {
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    // Cleanup: remove tray icon
    let nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: uid,
        ..Default::default()
    };
    unsafe {
        Shell_NotifyIconW(NIM_DELETE, &nid);
    }

    // Clean up the sender box
    unsafe {
        let _ = Box::from_raw(tx_ptr as *mut Sender<TrayCommand>);
    }

    info!("Tray thread exiting");
    Ok(())
}

/// Window procedure for the hidden notification window.
unsafe extern "system" fn tray_window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let callback_msg = WM_APP + 1;

    if msg == callback_msg {
        let event = (lparam as u32) & 0xffff;
        if event == WM_RBUTTONUP || event == WM_CONTEXTMENU {
            show_tray_menu(hwnd);
        } else if event == WM_LBUTTONUP {
            send_tray_command(hwnd, TrayCommand::Open);
        }
        return 0;
    }

    if msg == WM_DESTROY {
        PostQuitMessage(0);
        return 0;
    }

    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Show the tray context menu and handle the user's selection.
unsafe fn show_tray_menu(hwnd: HWND) {
    let menu = CreatePopupMenu();
    if menu == 0 {
        return;
    }

    let open_text = to_wide("Open");
    let exit_text = to_wide("Exit");

    let _ = AppendMenuW(
        menu,
        MF_STRING,
        IDM_OPEN as usize,
        PCWSTR(open_text.as_ptr()),
    );
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR(std::ptr::null()));
    let _ = AppendMenuW(
        menu,
        MF_STRING,
        IDM_EXIT as usize,
        PCWSTR(exit_text.as_ptr()),
    );

    // Required: set foreground so the menu can be dismissed by clicking away
    let _ = SetForegroundWindow(hwnd);

    let mut pt = POINT::default();
    GetCursorPos(&mut pt);

    let cmd = TrackPopupMenuEx(
        menu,
        TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
        pt.x,
        pt.y,
        hwnd,
        std::ptr::null_mut(),
    );

    // Post benign message so the menu finishes cleaning up
    let _ = PostMessageW(hwnd, WM_NULL, 0, 0);

    match cmd as u32 {
        IDM_OPEN => send_tray_command(hwnd, TrayCommand::Open),
        IDM_EXIT => send_tray_command(hwnd, TrayCommand::Exit),
        _ => {}
    }

    DestroyMenu(menu);
}

/// Send a tray command back to the main thread via the channel stored in GWLP_USERDATA.
unsafe fn send_tray_command(hwnd: HWND, cmd: TrayCommand) {
    let ptr = GetWindowLongPtrW(hwnd, GWL_USERDATA) as *mut Sender<TrayCommand>;
    if !ptr.is_null() {
        // try_send: a full queue (main thread busy) must not block the tray
        // message pump — a lost Open/Exit click is better than a frozen icon.
        let _ = (*ptr).try_send(cmd);
    }
}
