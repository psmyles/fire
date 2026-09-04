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
mod octagon;
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
        // Another Fire owns the socket: hand it the path and exit.
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            if let Err(e) = forward::forward(path) {
                eprintln!("fire: forward to running instance failed: {e}");
            }
            return;
        }
        // The OS refused the socket outright. Run anyway, un-coordinated: refusing to open an
        // image over a missing convenience would be the wrong trade.
        Err(e) => {
            eprintln!("fire: instance socket unavailable ({e}); running without it");
            None
        }
    };

    let event_loop = match EventLoop::<AppEvent>::with_user_event().build() {
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
        ipc_server::spawn(listener, proxy.clone());
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
