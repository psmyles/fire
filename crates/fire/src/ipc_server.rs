//! Named-pipe server (Option A from the plan): one background thread runs a blocking
//! pipe and forwards each `OpenRequest` to the UI thread by `PostMessage`-ing the window
//! with [`crate::win::WM_APP_OPEN`] and a boxed `OpenRequest` in the LPARAM. The thread
//! never touches the window or the renderer — it only posts.
//!
//! A single pipe instance is created once and reused across connections (Connect →
//! read → Disconnect → repeat). Because the pipe *name* therefore exists for as long as the
//! running instance is up, a forwarding launch never sees "not found" while it is up — at
//! worst a momentary `ERROR_PIPE_BUSY`, which the forwarder retries.

use std::fs::File;
use std::os::windows::io::{FromRawHandle, IntoRawHandle, RawHandle};
use std::ptr;

use fire_ipc::{read_message, OpenRequest, PIPE_NAME};

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_PIPE_CONNECTED, HWND, INVALID_HANDLE_VALUE,
};
// PIPE_ACCESS_DUPLEX lives under Storage::FileSystem (it's a file open-mode flag);
// CreateNamedPipeW additionally requires the Win32_Storage_FileSystem feature because
// its signature uses FILE_FLAGS_AND_ATTRIBUTES.
use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
    PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::win::WM_APP_OPEN;

const PIPE_BUFFER_SIZE: u32 = 64 * 1024;

/// Spawn the pipe-server thread. Each open request is posted to `hwnd` (the UI window),
/// passed as an `isize` so it crosses the thread boundary.
pub fn spawn(hwnd: isize) {
    std::thread::Builder::new()
        .name("fire-pipe-server".into())
        .spawn(move || run(hwnd))
        .expect("failed to spawn pipe-server thread");
}

fn run(hwnd: isize) {
    let name = wide(PIPE_NAME);
    let pipe = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            PIPE_ACCESS_DUPLEX,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            PIPE_UNLIMITED_INSTANCES,
            PIPE_BUFFER_SIZE,
            PIPE_BUFFER_SIZE,
            0,
            ptr::null(),
        )
    };
    if pipe == INVALID_HANDLE_VALUE {
        eprintln!(
            "fire: CreateNamedPipeW failed (err {})",
            unsafe { GetLastError() }
        );
        return;
    }

    loop {
        // Block until a client (a forwarding launch) connects.
        let connected = unsafe { ConnectNamedPipe(pipe, ptr::null_mut()) };
        if connected == 0 {
            let err = unsafe { GetLastError() };
            if err != ERROR_PIPE_CONNECTED {
                eprintln!("fire: ConnectNamedPipe failed (err {err})");
                unsafe { DisconnectNamedPipe(pipe) };
                continue;
            }
            // ERROR_PIPE_CONNECTED: the client connected before our ConnectNamedPipe
            // call — that's success, fall through and read.
        }

        // Read one framed message. Borrow the pipe handle via a File without taking
        // ownership (into_raw_handle releases it un-closed) so we can reuse the pipe.
        let mut file = unsafe { File::from_raw_handle(pipe as RawHandle) };
        let result = read_message(&mut file);
        let _ = file.into_raw_handle();

        match result {
            Ok(req) => {
                let boxed: Box<OpenRequest> = Box::new(req);
                let lparam = Box::into_raw(boxed) as isize;
                // SAFETY: the box outlives the post; the UI thread reclaims it in the
                // wndproc. Reclaim here if the post fails (window gone) so we don't leak.
                let posted = unsafe { PostMessageW(hwnd as HWND, WM_APP_OPEN, 0, lparam) };
                if posted == 0 {
                    drop(unsafe { Box::from_raw(lparam as *mut OpenRequest) });
                    break; // window is gone; stop serving
                }
            }
            Err(e) => eprintln!("fire: bad pipe message: {e}"),
        }

        unsafe { DisconnectNamedPipe(pipe) };
    }

    unsafe { CloseHandle(pipe) };
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
