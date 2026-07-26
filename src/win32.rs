//! Minimal Win32 FFI declarations — follows the existing `extern "system"` pattern
//! used in main.rs for CreateMutexW. Avoids pulling in the full `windows` crate.

#![cfg(windows)]

/// Opaque window handle (pointer-sized).
pub type HWND = isize;

pub const GWL_EXSTYLE: i32 = -20;
pub const WS_EX_TOOLWINDOW: isize = 0x00000080;
pub const WS_EX_APPWINDOW: isize = 0x00040000;

extern "system" {
    pub fn SetForegroundWindow(hWnd: HWND) -> i32;
    pub fn GetWindowLongPtrW(hWnd: HWND, nIndex: i32) -> isize;
    pub fn SetWindowLongPtrW(hWnd: HWND, nIndex: i32, dwNewLong: isize) -> isize;
}
