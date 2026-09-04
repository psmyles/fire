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
///
/// Two budgets, because the two failures mean different things. **The name is not there yet**
/// (`NotFound`, or a Windows pipe reporting every instance busy) is a genuine "not ready" and the
/// owner may be anywhere in its own startup, so it is worth waiting seconds for. **The name is
/// there and refuses** (`ConnectionRefused`) is a Unix socket file whose owner is gone; nothing is
/// ever going to start answering it. The only reason to retry that at all is the microscopic
/// window between an owner's `bind` and its `listen` — so the budget is short rather than zero:
/// long enough that a simultaneous launch cannot mistake a live owner for a corpse and steal the
/// socket from under it (see [`crate::ipc_server::rebind_after_stale`]), short enough that
/// recovering from a killed one is not a visible stall. It was the full two seconds before, which
/// is where the +2.0 s measured after every quit went.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const REFUSED_TIMEOUT: Duration = Duration::from_millis(150);

/// Forward `path` to the running owner, granting it foreground rights first (§4.1 of the
/// architecture notes — a Windows leaf; a no-op elsewhere). No-op if there is no path (a bare
/// launch with another instance up just exits).
pub fn forward(path: Option<PathBuf>) -> io::Result<()> {
    let Some(path) = path else {
        // Nothing to send — but the caller exits on `Ok`, so "an owner is already running" has to
        // be *true* here, not merely implied by the failed bind that got us here. Where the socket
        // is a file it outlives a crashed owner, so connect to find out: a refusal means the name
        // is stale and the caller must open a window instead of vanishing with no UI at all.
        connect_retry(CONNECT_TIMEOUT)?;
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
    let start = Instant::now();
    loop {
        match Stream::connect(socket_name()?) {
            Ok(s) => return Ok(s),
            Err(e) if start.elapsed() < budget(&e, timeout) => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => return Err(e),
        }
    }
}

/// How long `e` is worth retrying: the caller's full budget for "not there yet" (the owner is
/// still binding) or "busy" (every pipe instance is mid-accept), the short one for a refusal, and
/// none at all for anything else.
fn budget(e: &io::Error, timeout: Duration) -> Duration {
    /// `ERROR_PIPE_BUSY`: all pipe instances are momentarily busy; retry shortly.
    const ERROR_PIPE_BUSY: i32 = 231;
    if e.kind() == io::ErrorKind::NotFound || e.raw_os_error() == Some(ERROR_PIPE_BUSY) {
        timeout
    } else if e.kind() == io::ErrorKind::ConnectionRefused {
        REFUSED_TIMEOUT.min(timeout)
    } else {
        Duration::ZERO
    }
}
