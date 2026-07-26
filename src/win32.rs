//! Minimal Win32 FFI declarations — follows the existing `extern "system"` pattern
//! used in main.rs for CreateMutexW. Avoids pulling in the full `windows` crate.

#![cfg(windows)]

/// Opaque window handle (pointer-sized).
pub type HWND = isize;

pub const SW_HIDE: i32 = 0;
pub const SW_RESTORE: i32 = 9;

extern "system" {
    pub fn ShowWindow(hWnd: HWND, nCmdShow: i32) -> i32;
    pub fn SetForegroundWindow(hWnd: HWND) -> i32;
}
