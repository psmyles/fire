//! Single-instance forward path: when another Fire window already owns the pipe, hand it the
//! path and exit instead of opening a second window. Used only in `InstanceMode::SingleInstance`
//! — this is the former `fire-stub` client logic, folded into the one exe.

use std::fs::{File, OpenOptions};
use std::os::windows::io::AsRawHandle;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use fire_ipc::{write_message, OpenRequest, PIPE_NAME};

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
use windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow;

/// ERROR_PIPE_BUSY: all pipe instances are momentarily busy; retry shortly.
const ERROR_PIPE_BUSY: i32 = 231;
/// The owner may still be creating its pipe when we lose the mutex race; retry briefly.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// Forward `path` to the running owner, granting it foreground rights first (§4.1). No-op if
/// there is no path (a bare launch with another instance up just exits).
pub fn forward(path: Option<PathBuf>) -> std::io::Result<()> {
    let Some(path) = path else {
        return Ok(());
    };
    let req = OpenRequest::new(path);
    let mut file = connect_retry(CONNECT_TIMEOUT)?;
    if req.flags.activate {
        if let Ok(pid) = server_pid(&file) {
            // Best-effort: if the grant fails the open still works, it just may not raise.
            unsafe {
                AllowSetForegroundWindow(pid);
            }
        }
    }
    write_message(&mut file, &req).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
}

fn connect_once() -> std::io::Result<File> {
    OpenOptions::new().read(true).write(true).open(PIPE_NAME)
}

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

/// PID of the process on the server end of the connected pipe (the owning window).
fn server_pid(file: &File) -> std::io::Result<u32> {
    let mut pid: u32 = 0;
    let ok = unsafe { GetNamedPipeServerProcessId(file.as_raw_handle() as HANDLE, &mut pid) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(pid)
}

fn is_not_found(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::NotFound
}

fn is_busy(e: &std::io::Error) -> bool {
    e.raw_os_error() == Some(ERROR_PIPE_BUSY)
}
