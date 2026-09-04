//! The forward path: when another Fire already owns the instance socket, hand it the path and
//! exit instead of starting a second process. What the owner does with it (a new window, or the
//! focused one) is its `open-in` setting, not ours.

use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use interprocess::local_socket::{prelude::*, Stream};

use fire_ipc::{write_message, OpenRequest};

use crate::ipc_server::socket_name;

/// The owner may still be creating its socket when we lose the bind race; retry briefly.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// Forward `path` to the running owner, granting it foreground rights first (§4.1 of the
/// architecture notes — a Windows leaf; a no-op elsewhere). No-op if there is no path (a bare
/// launch with another instance up just exits).
pub fn forward(path: Option<PathBuf>) -> io::Result<()> {
    let Some(path) = path else {
        return Ok(());
    };
    let req = OpenRequest::new(path);
    let mut conn = connect_retry(CONNECT_TIMEOUT)?;
    if req.flags.activate {
        // Best-effort: if the grant fails the open still works, it just may not raise.
        crate::platform::grant_foreground_to_owner();
    }
    write_message(&mut conn, &req).map_err(io::Error::other)
}

fn connect_retry(timeout: Duration) -> io::Result<Stream> {
    let deadline = Instant::now() + timeout;
    loop {
        match Stream::connect(socket_name()?) {
            Ok(s) => return Ok(s),
            Err(e) if is_transient(&e) && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => return Err(e),
        }
    }
}

/// "Not there yet" (the owner is still binding) or "busy" (every pipe instance is mid-accept).
fn is_transient(e: &io::Error) -> bool {
    /// `ERROR_PIPE_BUSY`: all pipe instances are momentarily busy; retry shortly.
    const ERROR_PIPE_BUSY: i32 = 231;
    matches!(
        e.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    ) || e.raw_os_error() == Some(ERROR_PIPE_BUSY)
}
