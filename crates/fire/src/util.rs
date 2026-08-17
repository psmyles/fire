//! Small Win32 helpers shared across modules.

use windows_sys::Win32::Foundation::{GetLastError, ERROR_INVALID_WINDOW_HANDLE, HWND};
use windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW;

/// UTF-16, NUL-terminated, for Win32 `W` APIs.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// `%APPDATA%\fire` — where every persisted file (config.toml, window.toml) lives. One
/// definition, so the files cannot drift into different directories.
pub fn fire_dir() -> Option<std::path::PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(std::path::PathBuf::from(appdata).join("fire"))
}

/// What became of a [`post_boxed`] payload.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostOutcome {
    /// Ownership transferred to the UI thread; the wndproc reclaims the box.
    Posted,
    /// The post failed transiently — in practice `ERROR_NOT_ENOUGH_QUOTA`, the ~10k-message
    /// queue limit a held-down navigation key can reach. The payload was dropped here;
    /// the sender should keep serving.
    Dropped,
    /// The window no longer exists; there is no one left to post to and senders should stop.
    WindowGone,
}

/// Post `payload` to `hwnd` as the LPARAM of `msg`, transferring ownership of the box to the
/// receiving wndproc. On failure ownership never left this thread, so the box is reclaimed
/// here and the reason is classified — every background sender used to treat *any* failure
/// as "window gone" and retire itself, which turned one full-queue moment into a worker
/// (or the whole pipe server) silently gone for the rest of the session.
pub fn post_boxed<T>(hwnd: isize, msg: u32, payload: Box<T>) -> PostOutcome {
    let lparam = Box::into_raw(payload) as isize;
    // SAFETY: the box outlives the post; the UI thread reclaims it in the wndproc. On
    // failure, `Box::from_raw` takes ownership back of the pointer we just leaked.
    let posted = unsafe { PostMessageW(hwnd as HWND, msg, 0, lparam) };
    if posted != 0 {
        return PostOutcome::Posted;
    }
    let err = unsafe { GetLastError() };
    drop(unsafe { Box::from_raw(lparam as *mut T) });
    if err == ERROR_INVALID_WINDOW_HANDLE {
        PostOutcome::WindowGone
    } else {
        eprintln!("fire: PostMessageW({msg:#x}) failed (err {err}); payload dropped");
        PostOutcome::Dropped
    }
}
