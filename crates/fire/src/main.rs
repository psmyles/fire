//! fire — a native image viewer. One process, one event loop, N windows (see `app`).
//!
//! Launch: read the config, then try to become the instance by binding the local socket
//! ([`ipc_server::bind`]). The first launch succeeds and runs the event loop; every later one finds
//! the socket taken, forwards its path to the owner ([`forward`]) and exits. What the owner does
//! with a forwarded path — a new window, or the focused one — is the `open-in` setting. The bind
//! *is* the single-instance lock; there is no mutex.
//!
//! The launch path's decode is submitted before the event loop starts and before any window or GPU
//! exists, so the file is read while those come up: nothing waits on anything it does not need.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod chrome;
mod config;
mod decode_pool;
mod flipbook;
mod folder;
mod forward;
// Live-reload of `ui/theme.toml` into the running windows. Debug only — a release build has no
// source tree to watch and embeds the stylesheet instead.
#[cfg(debug_assertions)]
mod hotstyle;
mod icons;
mod ipc_server;
mod keybinds;
#[cfg(target_os = "macos")]
mod menubar;
mod octagon;
#[cfg(target_os = "macos")]
mod openfiles;
mod platform;
mod product;
mod render;
mod transport;
mod ttfp;
mod ui;
mod util;
mod watcher;
mod window_state;

use std::path::PathBuf;

use winit::event_loop::EventLoop;

use app::{AppEvent, Fire, Initial, MAX_CPU_DIM};
use config::Config;
use decode_pool::{fresh_generation, DecodeJob, DecodePool};

fn main() {
    // What it cost to reach the first line of `main`: the loader, the runtime, and nothing of
    // ours. Every other phase below is measured from an `Instant` taken inside the process and so
    // cannot see any of it — which is also why the TTFP stamp's origin is the kernel's
    // process-creation time rather than a mark taken here (see `ttfp`).
    render::gpu::report_timing(&format!(
        "process start → main — {:.2} ms",
        ttfp::ms_since_start()
    ));

    // The file manager passes the double-clicked file as the first argument.
    let path: Option<PathBuf> = std::env::args_os().nth(1).map(PathBuf::from);
    // Drop a commented config.toml on first run (no-op if one already exists), so the settings are
    // discoverable, then read it.
    config::ensure_default_config();
    let cfg = Config::load();

    // Start the GPU bring-up *now*, on its own thread — it needs no window, and it is the longest
    // single item on the launch path (see `render::gpu::Gpu::start`). It runs alongside
    // everything below and the first window joins it.
    let gpu = render::gpu::Gpu::start();

    let listener = match ipc_server::bind() {
        Ok(l) => Some(l),
        // Another Fire owns the socket: hand it the path and exit. (`AddrInUse` is the Unix
        // socket's answer; a Windows named pipe created with FILE_FLAG_FIRST_PIPE_INSTANCE says
        // `PermissionDenied` instead.) If the owner cannot be reached after all — it was exiting
        // as we launched — fall through and run un-coordinated rather than lose the open.
        Err(e) if ipc_server::is_taken(&e) => match forward::forward(path.clone()) {
            Ok(()) => return,
            Err(e) => {
                // Nobody answered a name that bind said was taken. The owner either exited as we
                // launched, or never cleaned up after being killed — either way the name is free
                // now, so reclaim it and serve, rather than running un-coordinated forever and
                // leaving every later launch to pay the same failed connect.
                eprintln!("fire: forward to running instance failed ({e}); opening here");
                ipc_server::rebind_after_stale().ok()
            }
        },
        // The OS refused the socket outright. Run anyway, un-coordinated: refusing to open an
        // image over a missing convenience would be the wrong trade.
        Err(e) => {
            eprintln!("fire: instance socket unavailable ({e}); running without it");
            None
        }
    };

    let mut event_loop_builder = EventLoop::<AppEvent>::with_user_event();
    // winit installs a default macOS menu bar of its own during `applicationDidFinishLaunching`,
    // which is *after* `main` gets to build one and would replace it wholesale. Turn it off so
    // Fire's menu (D16) is the one that survives. It is not a pure loss: winit's default is where
    // Cmd-Q came from before, so whatever replaces it has to carry Quit itself — `menubar` does.
    #[cfg(target_os = "macos")]
    {
        use winit::platform::macos::EventLoopBuilderExtMacOS as _;
        event_loop_builder.with_default_menu(false);
    }
    let event_loop = match event_loop_builder.build() {
        Ok(l) => l,
        Err(e) => {
            fatal_startup_error(&format!("fire could not create its event loop.\n\n{e}"));
            return;
        }
    };
    let proxy = event_loop.create_proxy();

    // Start the decode *now* — before the window, before the GPU — so the file is being read
    // while both come up. The first window adopts the request when it is created.
    let pool = DecodePool::new(proxy.clone());
    let initial = path.map(|path| {
        let generation = fresh_generation();
        pool.submit(DecodeJob {
            window: None,
            generation,
            path: path.clone(),
            opts: fire_decode::DecodeOptions {
                max_dim: MAX_CPU_DIM,
                honor_icc: true,
            },
            reload: false,
            detect_flipbook: cfg.flipbook.auto_detect,
        });
        Initial { path, generation }
    });

    if let Some(listener) = listener {
        // We are the owner, so we are the one that has to take the socket away again. Where the
        // socket is a file it outlives us otherwise, and ⌘Q — AppKit's `terminate:`, which ends in
        // `exit()` — never runs a destructor, so *every* normal quit would leave one behind and
        // cost the next launch the whole connect timeout. Registered only here, on the owning
        // branch: a forwarding launch deleting this file would be deleting someone else's.
        ipc_server::unlink_on_exit();
        ipc_server::spawn(listener, proxy.clone());
    }

    // The macOS menu bar (D16). Built before the loop runs but after it exists, because
    // `NSApplication` has to be up; held to the end of `main` because dropping the menu takes the
    // menu bar with it. A failure here is not worth refusing to show an image over — the app is
    // merely harder to quit — so it degrades to no menu with a note.
    #[cfg(target_os = "macos")]
    let _menu = {
        let binds = crate::keybinds::Keybinds::from_config(&cfg.keybinds);
        let menu = menubar::install(proxy.clone(), &binds, product::NAME);
        if menu.is_none() {
            eprintln!("fire: could not build the menu bar; Cmd-Q will not work");
        }
        menu
    };

    // Finder opens (D5/D6). macOS delivers a double-clicked file as an Apple event rather than as
    // an argument, so without this hook the bundle opens blank from Finder and a running Fire
    // ignores every later open. It must go in after the event loop is built — the delegate it
    // extends is winit's — and before the loop runs, because a launch-by-open fires early.
    #[cfg(target_os = "macos")]
    if !openfiles::install(proxy.clone()) {
        eprintln!("fire: could not hook Finder opens; only command-line paths will open");
    }

    let mut fire = Fire::new(cfg, proxy, pool, initial, gpu);
    if let Err(e) = event_loop.run_app(&mut fire) {
        eprintln!("fire: event loop error: {e}");
    }
}

/// Last-resort startup failure report. The release build has no console on Windows, so stderr
/// goes nowhere and a panic would be an invisible abort — a message box is the only channel the
/// user actually sees at this point.
fn fatal_startup_error(text: &str) {
    rfd::MessageDialog::new()
        .set_title(product::NAME)
        .set_level(rfd::MessageLevel::Error)
        .set_description(text)
        .show();
}
