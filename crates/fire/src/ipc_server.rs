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

/// The socket's name on this OS: a namespaced name where the OS has a namespace for them
/// (Windows named pipes, Linux abstract sockets), else a socket file under the user's runtime
/// directory (macOS).
pub fn socket_name() -> io::Result<Name<'static>> {
    if GenericNamespaced::is_supported() {
        SOCKET_NAME.to_ns_name::<GenericNamespaced>()
    } else {
        let dir = dirs::runtime_dir()
            .or_else(dirs::cache_dir)
            .unwrap_or_else(std::env::temp_dir);
        dir.join(SOCKET_NAME).to_fs_name::<GenericFilePath>()
    }
}

/// Try to become the instance owner. `Err(AddrInUse)` means another Fire already is — forward to
/// it. Any other error means the OS refused the socket outright; the caller runs without serving,
/// which degrades to "every launch is its own process" rather than refusing to open.
pub fn bind() -> io::Result<Listener> {
    ListenerOptions::new().name(socket_name()?).create_sync()
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
