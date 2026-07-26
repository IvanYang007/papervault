//! Minimal Win32 FFI declarations — follows the existing `extern "system"` pattern
//! used in main.rs for CreateMutexW. Avoids pulling in the full `windows` crate.

#![cfg(windows)]

/// Opaque window handle (pointer-sized).
pub type HWND = isize;

extern "system" {
    pub fn SetForegroundWindow(hWnd: HWND) -> i32;
}
