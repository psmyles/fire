//! The instance socket: one process, N windows.
//!
//! Every launch tries to **bind** the local socket ([`bind`]). The first one succeeds and becomes
//! the owner: it runs the event loop and serves the socket from one background thread
//! ([`spawn`]), turning each connection's [`OpenRequest`] into an [`AppEvent::Open`] for the
//! event loop, which opens the path in a new window or the focused one (the `open-in` setting).
//! A launch whose bind fails with `AddrInUse` is not the owner: it connects, forwards its path
//! ([`crate::forward`]) and exits. The bind *is* the mutex — there is no separate lock to race.
//!
//! The `interprocess` crate maps the one name onto a named pipe on Windows and a Unix socket
//! elsewhere; the wire format is `fire-ipc`'s and is unchanged. The serving thread never touches a
//! window or the renderer — it only sends events.

use std::io;

use interprocess::local_socket::{
    prelude::*, GenericFilePath, GenericNamespaced, Listener, ListenerOptions, Name,
};
use winit::event_loop::EventLoopProxy;

use fire_ipc::{read_message, SOCKET_NAME};

use crate::app::AppEvent;

/// Where the socket lives as a *file*, on the OSes that put it in the filesystem (macOS and other
/// Unixes without abstract sockets). `None` where the name lives in an OS namespace instead
/// (Windows named pipes, Linux abstract sockets) — there the kernel reclaims the name when the
/// owner dies, so there is nothing to clean up and nothing to go stale.
fn socket_file_path() -> Option<std::path::PathBuf> {
    if GenericNamespaced::is_supported() {
        return None;
    }
    let dir = dirs::runtime_dir()
        .or_else(dirs::cache_dir)
        .unwrap_or_else(std::env::temp_dir);
    Some(dir.join(SOCKET_NAME))
}

/// The socket's name on this OS: a namespaced name where the OS has a namespace for them
/// (Windows named pipes, Linux abstract sockets), else a socket file under the user's runtime
/// directory (macOS).
pub fn socket_name() -> io::Result<Name<'static>> {
    match socket_file_path() {
        None => SOCKET_NAME.to_ns_name::<GenericNamespaced>(),
        Some(path) => path.to_fs_name::<GenericFilePath>(),
    }
}

/// Delete a socket file whose owner is gone, so this launch can bind the name instead. Returns
/// whether anything was removed.
///
/// Only ever called once a forward has already *failed* to reach anyone, which is the proof that
/// the name is unowned — a live owner answers, so its socket is never removed here. This matters
/// only where the socket is a file: a Unix socket outlives the process that bound it, so an owner
/// killed with `SIGKILL` (or crashed, D7) leaves a file that no longer accepts connections but
/// still fails every future `bind` with `AddrInUse`. Left alone, that makes every later launch
/// stall on the connect timeout, and one with no path to forward exit without ever showing a
/// window. Windows has no equivalent: its named pipe disappears with its process.
///
/// Two launches can race here — both find the socket dead, both unlink, both bind. One wins; the
/// loser sees `AddrInUse` again and forwards to the winner, which is the ordinary path.
pub fn reclaim_stale() -> bool {
    let Some(path) = socket_file_path() else {
        return false;
    };
    match std::fs::remove_file(&path) {
        Ok(()) => {
            eprintln!(
                "fire: removed a stale instance socket at {} (its owner exited without cleaning \
                 up); taking ownership",
                path.display()
            );
            true
        }
        // Someone else got there first, or it was never a file we may remove. Either way this
        // launch simply runs without serving the socket.
        Err(_) => false,
    }
}

/// Try to become the instance owner. An error for which [`is_taken`] holds means another Fire
/// already is — forward to it. Any other error means the OS refused the socket outright; the
/// caller runs without serving, which degrades to "every launch is its own process" rather than
/// refusing to open.
pub fn bind() -> io::Result<Listener> {
    ListenerOptions::new().name(socket_name()?).create_sync()
}

/// Whether a [`bind`] error means "another instance holds the name". A Unix socket reports
/// `AddrInUse`; a Windows named pipe created with `FILE_FLAG_FIRST_PIPE_INSTANCE` (which is how
/// `interprocess` makes the first instance exclusive) fails with `ERROR_ACCESS_DENIED`, i.e.
/// `PermissionDenied`, when the name already exists.
pub fn is_taken(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::AddrInUse | io::ErrorKind::PermissionDenied
    )
}

/// Serve `listener` on a background thread for the life of the process, sending each forwarded
/// open to the event loop.
pub fn spawn(listener: Listener, proxy: EventLoopProxy<AppEvent>) {
    // Forwarding is a convenience; a thread the OS refused to start must not abort the viewer
    // before its window exists (the folder-scan and watcher threads already degrade this way).
    if let Err(e) = std::thread::Builder::new()
        .name("fire-instance-socket".into())
        .spawn(move || run(listener, proxy))
    {
        eprintln!("fire: could not start the instance-socket thread: {e}; forwarding disabled");
    }
}

fn run(listener: Listener, proxy: EventLoopProxy<AppEvent>) {
    for conn in listener.incoming() {
        let mut conn = match conn {
            Ok(c) => c,
            Err(e) => {
                eprintln!("fire: instance socket accept failed: {e}");
                // A handle that fails persistently would otherwise turn this loop into a
                // spinning core; failure is not a state worth polling at full speed.
                std::thread::sleep(std::time::Duration::from_millis(50));
                continue;
            }
        };
        // One framed message per connection; the client writes it immediately after connecting.
        match read_message(&mut conn) {
            Ok(req) => {
                // The only way a send fails is a closed event loop: the app is exiting.
                if proxy.send_event(AppEvent::Open(req)).is_err() {
                    return;
                }
            }
            Err(e) => eprintln!("fire: bad instance-socket message: {e}"),
        }
    }
}
