//! Named-pipe server (Option A from the plan): one background thread runs a blocking
//! pipe and forwards each `OpenRequest` to the winit event loop via `EventLoopProxy`.
//! The thread never touches wgpu or the window — it only `send_event`s.
//!
//! A single pipe instance is created once and reused across connections (Connect →
//! read → Disconnect → repeat). Because the pipe *name* therefore exists for the whole
//! daemon lifetime, a stub never sees "not found" while the daemon is up — at worst a
//! momentary `ERROR_PIPE_BUSY`, which the stub retries.

use std::fs::File;
use std::os::windows::io::{FromRawHandle, IntoRawHandle, RawHandle};
use std::ptr;

use texview_ipc::{read_message, PIPE_NAME};
use winit::event_loop::EventLoopProxy;

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_PIPE_CONNECTED, INVALID_HANDLE_VALUE,
};
// PIPE_ACCESS_DUPLEX lives under Storage::FileSystem (it's a file open-mode flag);
// CreateNamedPipeW additionally requires the Win32_Storage_FileSystem feature because
// its signature uses FILE_FLAGS_AND_ATTRIBUTES.
use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
    PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};

use crate::UserEvent;

const PIPE_BUFFER_SIZE: u32 = 64 * 1024;

/// Spawn the pipe-server thread. `proxy` wakes the event loop with each open request.
pub fn spawn(proxy: EventLoopProxy<UserEvent>) {
    std::thread::Builder::new()
        .name("texview-pipe-server".into())
        .spawn(move || run(proxy))
        .expect("failed to spawn pipe-server thread");
}

fn run(proxy: EventLoopProxy<UserEvent>) {
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
            "texview-daemon: CreateNamedPipeW failed (err {})",
            unsafe { GetLastError() }
        );
        return;
    }

    loop {
        // Block until a client (the stub) connects.
        let connected = unsafe { ConnectNamedPipe(pipe, ptr::null_mut()) };
        if connected == 0 {
            let err = unsafe { GetLastError() };
            if err != ERROR_PIPE_CONNECTED {
                eprintln!("texview-daemon: ConnectNamedPipe failed (err {err})");
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
                if proxy.send_event(UserEvent::Open(req)).is_err() {
                    break; // event loop is gone; stop serving
                }
            }
            Err(e) => eprintln!("texview-daemon: bad pipe message: {e}"),
        }

        unsafe { DisconnectNamedPipe(pipe) };
    }

    unsafe { CloseHandle(pipe) };
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
