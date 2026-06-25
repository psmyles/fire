//! Foreground activation (§4.1). A background process normally cannot raise its own
//! window: Windows ignores `SetForegroundWindow` from a process that doesn't own the
//! foreground. The stub works around this by calling `AllowSetForegroundWindow(daemon)`
//! just before sending the open request, handing us a one-shot grant. We must therefore
//! raise the window promptly on receipt, which is what this does.

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
