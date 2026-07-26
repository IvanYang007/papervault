//! Minimal Win32 FFI declarations — follows the existing `extern "system"` pattern
//! used in main.rs for CreateMutexW. Avoids pulling in the full `windows` crate.

#![cfg(windows)]
#![allow(non_snake_case, dead_code, clippy::upper_case_acronyms)]

use std::ffi::c_void;

pub type HWND = isize;
pub type HINSTANCE = isize;
pub type HMENU = isize;
pub type HICON = isize;
pub type HBRUSH = isize;
pub type HCURSOR = isize;
pub type WPARAM = usize;
pub type LPARAM = isize;
pub type LRESULT = isize;
pub type LPCWSTR = *const u16;
pub type BOOL = i32;
pub type UINT = u32;
pub type DWORD = u32;
pub type LONG = i32;
pub type ATOM = u16;

#[repr(C)]
#[derive(Default)]
pub struct POINT {
    pub x: i32,
    pub y: i32,
}

#[repr(C)]
pub struct MSG {
    pub hwnd: HWND,
    pub message: UINT,
    pub wParam: WPARAM,
    pub lParam: LPARAM,
    pub time: DWORD,
    pub pt: POINT,
}

impl Default for MSG {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

#[repr(C)]
pub struct WNDCLASSEXW {
    pub cbSize: UINT,
    pub style: UINT,
    pub lpfnWndProc: Option<unsafe extern "system" fn(HWND, UINT, WPARAM, LPARAM) -> LRESULT>,
    pub cbClsExtra: i32,
    pub cbWndExtra: i32,
    pub hInstance: HINSTANCE,
    pub hIcon: HICON,
    pub hCursor: HCURSOR,
    pub hbrBackground: HBRUSH,
    pub lpszMenuName: *const u16,
    pub lpszClassName: *const u16,
    pub hIconSm: HICON,
}

impl Default for WNDCLASSEXW {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

#[repr(C)]
pub struct NOTIFYICONDATAW {
    pub cbSize: DWORD,
    pub hWnd: HWND,
    pub uID: UINT,
    pub uFlags: UINT,
    pub uCallbackMessage: UINT,
    pub hIcon: HICON,
    pub szTip: [u16; 128],
    pub dwState: DWORD,
    pub dwStateMask: DWORD,
    pub szInfo: [u16; 256],
    pub uVersion_union: UINT,
    pub szInfoTitle: [u16; 64],
    pub dwInfoFlags: DWORD,
    pub guidItem: [u8; 16],
    pub hBalloonIcon: HICON,
}

impl Default for NOTIFYICONDATAW {
    fn default() -> Self {
        // SAFETY: zeroed memory is valid for this struct
        unsafe { std::mem::zeroed() }
    }
}

// Window styles
pub const CS_HREDRAW: UINT = 0x0002;
pub const CS_VREDRAW: UINT = 0x0001;
pub const WS_OVERLAPPED: DWORD = 0x00000000;

// Extended window styles
pub const GWL_USERDATA: i32 = -21;
pub const GWL_EXSTYLE: i32 = -20;
pub const WS_EX_TOOLWINDOW: isize = 0x00000080;
pub const WS_EX_APPWINDOW: isize = 0x00040000;

// ShowWindow
pub const SW_SHOW: i32 = 5;

// SetWindowPos
pub const SWP_FRAMECHANGED: UINT = 0x0020;
pub const SWP_NOMOVE: UINT = 0x0002;
pub const SWP_NOSIZE: UINT = 0x0001;
pub const SWP_NOZORDER: UINT = 0x0004;
pub const SWP_NOACTIVATE: UINT = 0x0010;

// Messages
pub const WM_NULL: UINT = 0x0000;
pub const WM_DESTROY: UINT = 0x0002;
pub const WM_RBUTTONUP: UINT = 0x0205;
pub const WM_LBUTTONUP: UINT = 0x0202;
pub const WM_CONTEXTMENU: UINT = 0x007B;
pub const WM_APP: UINT = 0x8000;

// Shell_NotifyIcon
pub const NIM_ADD: DWORD = 0x00000000;
pub const NIM_DELETE: DWORD = 0x00000002;
pub const NIF_ICON: UINT = 0x00000002;
pub const NIF_MESSAGE: UINT = 0x00000001;
pub const NIF_TIP: UINT = 0x00000004;

// Menu
pub const MF_STRING: UINT = 0x00000000;
pub const MF_SEPARATOR: UINT = 0x00000800;

// TrackPopupMenu
pub const TPM_RIGHTBUTTON: UINT = 0x0002;
pub const TPM_RETURNCMD: UINT = 0x0100;
pub const TPM_NONOTIFY: UINT = 0x0080;

// LoadImage
pub const IMAGE_ICON: UINT = 1;
pub const LR_LOADFROMFILE: UINT = 0x00000010;
pub const LR_DEFAULTSIZE: UINT = 0x00000040;

// Window creation
pub const CW_USEDEFAULT: i32 = 0x80000000u32 as i32;

// Struct with a pointer field used for PCWSTR
#[repr(transparent)]
pub struct PCWSTR(pub *const u16);

// WINDOW_EX_STYLE
#[derive(Default)]
#[allow(non_camel_case_types)]
#[repr(transparent)]
pub struct WINDOW_EX_STYLE(pub DWORD);

pub type HMODULE = *mut c_void;

extern "system" {
    pub fn GetModuleHandleW(lpModuleName: *const u16) -> HMODULE;
    pub fn RegisterClassExW(lpWndClass: *const WNDCLASSEXW) -> ATOM;
    pub fn CreateWindowExW(
        dwExStyle: WINDOW_EX_STYLE,
        lpClassName: PCWSTR,
        lpWindowName: PCWSTR,
        dwStyle: DWORD,
        X: i32,
        Y: i32,
        nWidth: i32,
        nHeight: i32,
        hWndParent: HWND,
        hMenu: HMENU,
        hInstance: HINSTANCE,
        lpParam: *mut c_void,
    ) -> HWND;
    pub fn DefWindowProcW(hwnd: HWND, msg: UINT, wParam: WPARAM, lParam: LPARAM) -> LRESULT;
    pub fn GetMessageW(
        lpMsg: *mut MSG,
        hWnd: HWND,
        wMsgFilterMin: UINT,
        wMsgFilterMax: UINT,
    ) -> BOOL;
    pub fn TranslateMessage(lpMsg: *const MSG) -> BOOL;
    pub fn DispatchMessageW(lpMsg: *const MSG) -> LRESULT;
    pub fn PostQuitMessage(nExitCode: i32);
    pub fn PostMessageW(hWnd: HWND, Msg: UINT, wParam: WPARAM, lParam: LPARAM) -> BOOL;
    pub fn GetWindowLongPtrW(hWnd: HWND, nIndex: i32) -> isize;
    pub fn SetWindowLongPtrW(hWnd: HWND, nIndex: i32, dwNewLong: isize) -> isize;
    pub fn SetWindowPos(
        hWnd: HWND,
        hWndInsertAfter: HWND,
        X: i32,
        Y: i32,
        cx: i32,
        cy: i32,
        uFlags: UINT,
    ) -> BOOL;
    pub fn ShowWindow(hWnd: HWND, nCmdShow: i32) -> BOOL;
    pub fn SetForegroundWindow(hWnd: HWND) -> BOOL;
    pub fn Shell_NotifyIconW(dwMessage: DWORD, lpData: *const NOTIFYICONDATAW) -> BOOL;
    pub fn LoadImageW(
        hInst: HINSTANCE,
        name: PCWSTR,
        typ: UINT,
        cx: i32,
        cy: i32,
        fuLoad: UINT,
    ) -> HMODULE;
    pub fn CreatePopupMenu() -> HMENU;
    pub fn AppendMenuW(hMenu: HMENU, uFlags: UINT, uIDNewItem: usize, lpNewItem: PCWSTR) -> BOOL;
    pub fn DestroyMenu(hMenu: HMENU) -> BOOL;
    pub fn TrackPopupMenuEx(
        hMenu: HMENU,
        uFlags: UINT,
        x: i32,
        y: i32,
        hWnd: HWND,
        lptpm: *mut c_void,
    ) -> BOOL;
    pub fn GetCursorPos(lpPoint: *mut POINT) -> BOOL;
}
