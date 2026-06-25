//! fire-daemon — the resident process.
//!
//! Startup: acquire the single-instance mutex (exit if another daemon owns it), build
//! the winit event loop with a custom `UserEvent`, spawn the pipe-server thread (which
//! wakes the loop on each open request), warm the GPU, and run. The pooled window is
//! created hidden in `resumed` and shown on the first open.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod decode_pool;
mod foreground;
mod gpu;
mod ipc_server;

use std::ptr;

use fire_ipc::{OpenRequest, MUTEX_NAME};
use winit::event_loop::{ControlFlow, EventLoop};

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
use windows_sys::Win32::System::Threading::CreateMutexW;

/// Custom events delivered into the winit event loop from background threads.
#[derive(Debug)]
pub enum UserEvent {
    /// An open request from the pipe-server thread (a stub forwarded a path).
    Open(OpenRequest),
    /// A finished decode from a worker thread, ready to upload (or an error).
    DecodeDone(decode_pool::DecodeOutcome),
}

fn main() {
    // Single-instance: a second daemon detects the mutex and exits quietly.
    let _instance = match SingleInstance::acquire() {
        Some(guard) => guard,
        None => return,
    };

    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .expect("failed to build event loop");
    // Idle, event-driven: ~0% CPU until a pipe message or redraw wakes us.
    event_loop.set_control_flow(ControlFlow::Wait);

    // Hand the pipe server a proxy so it can wake the loop with each open request.
    ipc_server::spawn(event_loop.create_proxy());

    // The decode workers post results back through their own proxy.
    let pool = decode_pool::DecodePool::new(event_loop.create_proxy());

    let mut app = app::App::new(pool);
    event_loop.run_app(&mut app).expect("event loop error");
}

/// Holds the single-instance mutex for the process lifetime.
struct SingleInstance(HANDLE);

impl SingleInstance {
    /// Returns `Some` if we are the first daemon, `None` if another already holds it.
    fn acquire() -> Option<Self> {
        let name: Vec<u16> = MUTEX_NAME.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: name is a valid null-terminated wide string; null attributes are fine.
        let handle = unsafe { CreateMutexW(ptr::null(), 1 /* initial owner */, name.as_ptr()) };
        if handle.is_null() {
            // Couldn't create the mutex; proceed without the guarantee rather than
            // refuse to start.
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
