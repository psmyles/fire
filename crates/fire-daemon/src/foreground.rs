//! Foreground activation (§4.1). A background process normally cannot raise its own
//! window: Windows ignores `SetForegroundWindow` from a process that doesn't own the
//! foreground. The stub works around this by calling `AllowSetForegroundWindow(daemon)`
//! just before sending the open request, handing us a one-shot grant. We must therefore
//! raise the window promptly on receipt, which is what this does.

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    SetForegroundWindow, ShowWindow, SW_RESTORE, SW_SHOW,
};
use winit::window::Window;

/// Show, un-minimize, and bring `window` to the foreground.
pub fn raise(window: &Window) {
    let Some(hwnd) = hwnd_of(window) else {
        return;
    };
    // SAFETY: hwnd is a live top-level window handle owned by this process.
    unsafe {
        ShowWindow(hwnd, SW_RESTORE); // restore if minimized
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
    }
}

fn hwnd_of(window: &Window) -> Option<HWND> {
    match window.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get() as HWND),
        _ => None,
    }
}
