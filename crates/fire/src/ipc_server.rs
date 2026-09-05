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

/// The socket file's path, on the OSes where the socket *is* a file. `None` where the name lives
/// in a kernel namespace instead (Windows named pipes, Linux abstract sockets) and there is no
/// file to speak of.
///
/// Under the user's cache directory rather than a shared temp dir: a socket in `/tmp` is one name
/// for the whole machine, so two people logged into the same Mac would fight over it, and the
/// second would be told the instance was "already running" by a process it cannot see.
pub fn socket_file_path() -> Option<std::path::PathBuf> {
    if namespace_is_the_kernel_s() {
        return None;
    }
    let dir = dirs::runtime_dir()
        .or_else(dirs::cache_dir)
        .unwrap_or_else(std::env::temp_dir);
    Some(dir.join(SOCKET_NAME))
}

/// Whether this OS's local-socket namespace is the *kernel's* — so the name disappears when its
/// owner does, with no file left behind.
///
/// `GenericNamespaced::is_supported()` alone is not that question. It answers "can I pass a bare
/// name", and on macOS it says yes and then `interprocess` emulates the namespace with a file in
/// the temp directory — a name that outlives its owner while claiming not to. Everything that
/// follows a crashed owner ([`rebind_after_stale`], [`unlink_on_exit`]) turns on the distinction,
/// so it is asked here rather than inferred.
fn namespace_is_the_kernel_s() -> bool {
    GenericNamespaced::is_supported() && !cfg!(target_vendor = "apple")
}

/// The socket's name on this OS: a namespaced name where the kernel owns the namespace, else an
/// explicit socket file path.
pub fn socket_name() -> io::Result<Name<'static>> {
    match socket_file_path() {
        None => SOCKET_NAME.to_ns_name::<GenericNamespaced>(),
        Some(path) => path.to_fs_name::<GenericFilePath>(),
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

/// Bind, displacing a socket whose owner is gone.
///
/// **Only call this once a forward has already failed to reach anyone**, which is the proof that
/// nothing is listening. `try_overwrite` is off by default for good reason: it deletes the socket
/// on `AddrInUse` *whether or not* someone is still accepting on it, so calling it speculatively
/// would let a second launch steal the name from a live owner and break the one-process model.
///
/// Why this is needed at all: on Unix a socket is a file that outlives the process that bound it,
/// so an owner killed with `SIGKILL` — or crashed, which D7 accepts — leaves a file that answers
/// nothing but still fails every later `bind` with `AddrInUse`. Left alone, that made every launch
/// stall on the connect timeout and, with no path to forward, exit without ever showing a window.
///
/// `try_overwrite` does the deleting rather than an `unlink` of our own, so the two paths that
/// remove the socket cannot disagree about where it is.
pub fn rebind_after_stale() -> io::Result<Listener> {
    ListenerOptions::new()
        .name(socket_name()?)
        .try_overwrite(true)
        .create_sync()
}

/// Have the socket file removed when this process exits, however it exits.
///
/// **Only the owner may call this**, and only after its own bind succeeded: a launch that merely
/// *forwards* would otherwise delete the socket of the instance it just talked to.
///
/// Rust's `Drop` is not enough on macOS, and the gap is not an edge case. ⌘Q is AppKit's
/// `terminate:`, which ends in `exit()` — `main` never returns, the [`Listener`] is never dropped,
/// and the socket file survives its owner. The next launch then finds the name taken, spends the
/// whole connect timeout discovering that nobody answers, and only then reclaims it: measured at
/// **+2.0 s on every launch after a normal quit** (168 ms → 2196 ms) before this existed. `atexit`
/// runs on that path, on a plain `main` return, and on the [`crate::ttfp`] stamp's `exit(0)`.
///
/// What it deliberately does not cover is `SIGKILL` and a hard crash, where no user code runs at
/// all. [`rebind_after_stale`] is still the answer there — this only makes it the rare path it was
/// meant to be.
#[cfg(unix)]
pub fn unlink_on_exit() {
    let Some(path) = socket_file_path() else {
        return; // A kernel-owned name needs no help.
    };
    // The handler gets no arguments, so the path has to reach it through a static. Written once,
    // before the handler can run, and only read afterwards.
    static PATH: std::sync::OnceLock<std::ffi::CString> = std::sync::OnceLock::new();
    let Ok(c_path) = std::ffi::CString::new(path.into_os_string().into_encoded_bytes()) else {
        return; // A path with an interior NUL is not one we created.
    };
    if PATH.set(c_path).is_err() {
        return; // Already registered; registering twice would unlink twice.
    }
    extern "C" fn unlink_socket() {
        if let Some(path) = PATH.get() {
            // SAFETY: a NUL-terminated path that outlives the call. `atexit` handlers run on the
            // exiting thread with the process otherwise winding down, so this is not a signal
            // context and an ordinary libc call is fine. A failure means it is already gone.
            unsafe { libc::unlink(path.as_ptr()) };
        }
    }
    // SAFETY: registering a plain `extern "C" fn` with no arguments, which is exactly what
    // `atexit` takes.
    unsafe { libc::atexit(unlink_socket) };
}

/// The twin for the OSes whose local-socket name the kernel owns — nothing to unlink, so nothing
/// to register. Windows is the only one `fire` ships, and its named pipe dies with the process.
#[cfg(not(unix))]
pub fn unlink_on_exit() {}

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
