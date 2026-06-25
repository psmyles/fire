//! fire — a native Win32 image viewer (single self-coordinating exe; Explorer launches it
//! directly with the image path).
//!
//! Lifecycle is chosen by config (`InstanceMode`):
//!   * **NewWindow** (default): just open our own window for the path. No mutex, no pipe,
//!     nothing listening — each launch is an independent process that exits when its window
//!     closes.
//!   * **SingleInstance**: acquire a mutex. If we win, open the window AND serve a pipe so
//!     later launches reuse this window; if another instance already owns it, forward the
//!     path to it over the pipe and exit. The pipe lives only inside the running window's
//!     process — it is not a resident daemon.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod chrome;
mod config;
mod decode_pool;
mod forward;
mod foreground;
mod ipc_server;
mod render;
mod win;

use std::path::PathBuf;
use std::ptr;

use config::{Config, InstanceMode};
use fire_ipc::MUTEX_NAME;

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};

fn main() {
    // Declare Per-Monitor-V2 DPI awareness *before* any window exists, so the OS never
    // bitmap-stretches us: the title bar/non-client area auto-scale, WM_DPICHANGED fires on
    // monitor moves, and our client chrome scales from GetDpiForWindow. (Doing this in code
    // rather than a manifest keeps it in one place and avoids manifest-embedding plumbing.)
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    // Explorer passes the double-clicked file as the first argument.
    let path: Option<PathBuf> = std::env::args_os().nth(1).map(PathBuf::from);
    let cfg = Config::load();

    match cfg.instance_mode {
        InstanceMode::NewWindow => {
            // Plain app: our own window, no coordination of any kind.
            win::run(path, false);
        }
        InstanceMode::SingleInstance => match SingleInstance::acquire() {
            // We're the owner: open the window and serve the pipe for later launches.
            Some(_guard) => win::run(path, true),
            // Another window owns the pipe: hand it the path and exit.
            None => {
                if let Err(e) = forward::forward(path) {
                    eprintln!("fire: forward to running instance failed: {e}");
                }
            }
        },
    }
}

/// Holds the single-instance mutex for the process lifetime (SingleInstance mode only).
struct SingleInstance(HANDLE);

impl SingleInstance {
    /// Returns `Some` if we are the first instance, `None` if another already holds it.
    fn acquire() -> Option<Self> {
        let name: Vec<u16> = MUTEX_NAME.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: name is a valid null-terminated wide string; null attributes are fine.
        let handle = unsafe { CreateMutexW(ptr::null(), 1 /* initial owner */, name.as_ptr()) };
        if handle.is_null() {
            // Couldn't create the mutex; proceed without the guarantee rather than refuse.
            return Some(SingleInstance(ptr::null_mut()));
        }
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            unsafe { CloseHandle(handle) };
            return None;
        }
        Some(SingleInstance(handle))
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CloseHandle(self.0) };
        }
    }
}
