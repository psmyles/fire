//! Foreground activation. A process that doesn't own the foreground normally cannot raise
//! its own window: Windows ignores `SetForegroundWindow` from it. In single-instance mode a
//! later launch (which Explorer gave the foreground) works around this by calling
//! `AllowSetForegroundWindow(owner_pid)` just before forwarding the open request, handing the
//! running instance a one-shot grant. We must therefore raise the window promptly on receipt,
//! which is what this does.

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    IsIconic, SetForegroundWindow, ShowWindow, SW_RESTORE, SW_SHOW,
};

/// Show, un-minimize, and bring the window (a raw HWND, as `isize`) to the foreground.
pub fn raise(hwnd: isize) {
    let hwnd = hwnd as HWND;
    // SAFETY: hwnd is a live top-level window handle owned by this process.
    unsafe {
        // Un-minimize *only* when actually minimized: `SW_RESTORE` also restores a **maximized**
        // window to its windowed size, so calling it unconditionally would un-maximize the window
        // on every activating open (a forwarded open, or File→Open in this window). `SW_SHOW`
        // leaves the maximized state alone, so a maximized window stays maximized.
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        }
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
    }
}
