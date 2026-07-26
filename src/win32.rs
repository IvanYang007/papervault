//! Minimal Win32 FFI declarations — follows the existing `extern "system"` pattern
//! used in main.rs for CreateMutexW. Avoids pulling in the full `windows` crate.

#![cfg(windows)]

/// Opaque window handle (pointer-sized).
pub type HWND = isize;

pub const GWL_EXSTYLE: i32 = -20;
pub const WS_EX_TOOLWINDOW: isize = 0x00000080;
pub const WS_EX_APPWINDOW: isize = 0x00040000;
pub const SWP_FRAMECHANGED: u32 = 0x0020;
pub const SWP_NOMOVE: u32 = 0x0002;
pub const SWP_NOSIZE: u32 = 0x0001;
pub const SWP_NOZORDER: u32 = 0x0004;
pub const SWP_NOACTIVATE: u32 = 0x0010;

extern "system" {
    pub fn SetForegroundWindow(hWnd: HWND) -> i32;
    pub fn GetWindowLongPtrW(hWnd: HWND, nIndex: i32) -> isize;
    pub fn SetWindowLongPtrW(hWnd: HWND, nIndex: i32, dwNewLong: isize) -> isize;
    pub fn SetWindowPos(
        hWnd: HWND,
        hWndInsertAfter: HWND,
        X: i32, Y: i32, cx: i32, cy: i32,
        uFlags: u32,
    ) -> i32;
}
