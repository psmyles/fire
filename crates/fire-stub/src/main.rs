//! fire-stub — the tiny launcher Explorer invokes.
//!
//! Flow (§3, §4.1):
//!   1. Try to connect to the resident daemon's named pipe (opened as a plain file).
//!   2. If absent, spawn the daemon and retry until the pipe appears.
//!   3. Resolve the daemon's PID and call `AllowSetForegroundWindow(pid)` so the
//!      daemon — a background process that otherwise cannot raise itself — may bring
//!      its window to the foreground (the granted right is one-shot, so we do this
//!      immediately before sending and then exit promptly).
//!   4. Send one `OpenRequest` and exit.
//!
//! The stub must stay trivial: no heavy deps, no GPU, no runtime to warm.

// Release builds are a GUI subsystem app (no console flash on double-click); debug
// keeps the console so dev output is visible.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::fs::{File, OpenOptions};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use fire_ipc::{write_message, OpenRequest, PIPE_NAME};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Pipes::GetNamedPipeServerProcessId;
use windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow;

/// ERROR_PIPE_BUSY: all pipe instances are busy; retry shortly.
const ERROR_PIPE_BUSY: i32 = 231;
/// CreateProcess flag: don't allocate a console for the spawned daemon.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// How long to wait for a freshly-spawned daemon to create its pipe.
const SPAWN_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

fn main() {
    let path = match std::env::args_os().nth(1) {
        Some(p) => PathBuf::from(p),
        None => {
            eprintln!("usage: fire-stub <image-path>");
            std::process::exit(2);
        }
    };

    let req = OpenRequest::new(path);
    if let Err(e) = run(&req) {
        eprintln!("fire-stub: {e}");
        std::process::exit(1);
    }
}

fn run(req: &OpenRequest) -> std::io::Result<()> {
    // Happy path: a daemon is already listening.
    let file = match connect_once() {
        Ok(f) => f,
        Err(e) if is_not_found(&e) => {
            // Cold start: no daemon yet. Spawn one and wait for its pipe.
            spawn_daemon()?;
            connect_retry(SPAWN_CONNECT_TIMEOUT)?
        }
        Err(e) if is_busy(&e) => connect_retry(SPAWN_CONNECT_TIMEOUT)?,
        Err(e) => return Err(e),
    };

    send(file, req)
}

/// Open the daemon's named pipe as a read/write file. NotFound means no daemon;
/// ERROR_PIPE_BUSY means a daemon exists but every pipe instance is momentarily busy.
fn connect_once() -> std::io::Result<File> {
    OpenOptions::new().read(true).write(true).open(PIPE_NAME)
}

/// Retry connecting until `timeout` elapses, sleeping briefly between attempts.
fn connect_retry(timeout: Duration) -> std::io::Result<File> {
    let deadline = Instant::now() + timeout;
    loop {
        match connect_once() {
            Ok(f) => return Ok(f),
            Err(e) if (is_not_found(&e) || is_busy(&e)) && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => return Err(e),
        }
    }
}

/// Grant foreground rights to the daemon, then send the request and exit. Order
/// matters: the foreground grant is one-shot and lapses on the next foreground
/// change, so we grant immediately before writing and keep the tail tight (§4.1).
fn send(mut file: File, req: &OpenRequest) -> std::io::Result<()> {
    if req.flags.activate {
        if let Ok(pid) = server_pid(&file) {
            // Best-effort: if the grant fails the open still works, it just may not
            // come to the foreground.
            unsafe {
                let _ = AllowSetForegroundWindow(pid);
            }
        }
    }

    write_message(&mut file, req).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
}

/// Query the PID of the process on the server end of the connected pipe (the daemon).
fn server_pid(file: &File) -> windows::core::Result<u32> {
    let handle = HANDLE(file.as_raw_handle() as *mut _);
    let mut pid: u32 = 0;
    unsafe { GetNamedPipeServerProcessId(handle, &mut pid)? };
    Ok(pid)
}

/// Spawn the resident daemon (a sibling exe in the same directory as this stub).
fn spawn_daemon() -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    let dir = exe.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::Other, "stub has no parent directory")
    })?;
    let daemon = dir.join("fire-daemon.exe");

    // Spawn detached: the Child handle is dropped, but dropping it does not kill the
    // process, so the daemon survives this stub's exit.
    Command::new(&daemon)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map(|_child| ())
}

fn is_not_found(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::NotFound
}

fn is_busy(e: &std::io::Error) -> bool {
    e.raw_os_error() == Some(ERROR_PIPE_BUSY)
}
