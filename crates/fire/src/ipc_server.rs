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
    CloseHandle, GetLastError, ERROR_PIPE_CONNECTED, INVALID_HANDLE_VALUE,
};
// PIPE_ACCESS_DUPLEX lives under Storage::FileSystem (it's a file open-mode flag);
// CreateNamedPipeW additionally requires the Win32_Storage_FileSystem feature because
// its signature uses FILE_FLAGS_AND_ATTRIBUTES.
use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PeekNamedPipe, PIPE_READMODE_BYTE,
    PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};

use crate::util::{post_boxed, wide, PostOutcome};
use crate::win::WM_APP_OPEN;

const PIPE_BUFFER_SIZE: u32 = 64 * 1024;

/// Spawn the pipe-server thread. Each open request is posted to `hwnd` (the UI window),
/// passed as an `isize` so it crosses the thread boundary.
pub fn spawn(hwnd: isize) {
    // Forwarding is a convenience; a thread the OS refused to start must not abort the viewer
    // before its window exists (the folder-scan and watcher threads already degrade this way).
    if let Err(e) = std::thread::Builder::new()
        .name("fire-pipe-server".into())
        .spawn(move || run(hwnd))
    {
        eprintln!("fire: could not start the pipe-server thread: {e}; forwarding disabled");
    }
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
        eprintln!("fire: CreateNamedPipeW failed (err {})", unsafe {
            GetLastError()
        });
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
                // A handle that fails persistently would otherwise turn this loop into a
                // spinning core; failure is not a state worth polling at full speed.
                std::thread::sleep(std::time::Duration::from_millis(50));
                continue;
            }
            // ERROR_PIPE_CONNECTED: the client connected before our ConnectNamedPipe
            // call — that's success, fall through and read.
        }

        // A client that connects and then never writes would wedge this single pipe instance
        // forever — the read below is blocking with no timeout, and every later forwarding
        // launch would burn its retry window and give up. A real forwarder writes its message
        // immediately after connecting, so wait briefly for the first bytes and disconnect
        // anyone who sends nothing. (A client that dies unblocks the read with a broken-pipe
        // error on its own; this handles the one that stays connected and silent.)
        let mut waited = std::time::Duration::ZERO;
        const PATIENCE: std::time::Duration = std::time::Duration::from_secs(2);
        const POLL: std::time::Duration = std::time::Duration::from_millis(20);
        let ready = loop {
            let mut avail = 0u32;
            let ok = unsafe {
                PeekNamedPipe(
                    pipe,
                    ptr::null_mut(),
                    0,
                    ptr::null_mut(),
                    &mut avail,
                    ptr::null_mut(),
                )
            };
            if ok == 0 || avail > 0 {
                break ok != 0; // bytes ready, or the peek itself failed (read will report why)
            }
            if waited >= PATIENCE {
                break false;
            }
            std::thread::sleep(POLL);
            waited += POLL;
        };
        if !ready {
            eprintln!("fire: pipe client sent nothing; disconnecting it");
            unsafe { DisconnectNamedPipe(pipe) };
            continue;
        }

        // Read one framed message. Borrow the pipe handle via a File without taking
        // ownership (into_raw_handle releases it un-closed) so we can reuse the pipe.
        let mut file = unsafe { File::from_raw_handle(pipe as RawHandle) };
        let result = read_message(&mut file);
        let _ = file.into_raw_handle();

        match result {
            Ok(req) => {
                let boxed: Box<OpenRequest> = Box::new(req);
                if post_boxed(hwnd, WM_APP_OPEN, boxed) == PostOutcome::WindowGone {
                    break; // window is gone; stop serving
                }
            }
            Err(e) => eprintln!("fire: bad pipe message: {e}"),
        }

        unsafe { DisconnectNamedPipe(pipe) };
    }

    unsafe { CloseHandle(pipe) };
}
