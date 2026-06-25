//! Foreground activation. A process that doesn't own the foreground normally cannot raise
//! its own window: Windows ignores `SetForegroundWindow` from it. In single-instance mode a
//! later launch (which Explorer gave the foreground) works around this by calling
//! `AllowSetForegroundWindow(owner_pid)` just before forwarding the open request, handing the
//! running instance a one-shot grant. We must therefore raise the window promptly on receipt,
//! which is what this does.

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    SetForegroundWindow, ShowWindow, SW_RESTORE, SW_SHOW,
};

/// Show, un-minimize, and bring the window (a raw HWND, as `isize`) to the foreground.
pub fn raise(hwnd: isize) {
    let hwnd = hwnd as HWND;
    // SAFETY: hwnd is a live top-level window handle owned by this process.
    unsafe {
        ShowWindow(hwnd, SW_RESTORE); // restore if minimized
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
    }
}
