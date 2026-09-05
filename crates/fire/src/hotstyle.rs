//! Live-reload of the stylesheet — **debug builds only** (`main.rs` doesn't declare this module in
//! release, so none of it is compiled in).
//!
//! A thread watches `crates/fire/src/ui/theme.toml` in the *source tree* (its path is baked in at
//! compile time by [`crate::ui::theme::SOURCE_PATH`]). On a change it re-parses the stylesheet,
//! installs it if it is valid, and sends [`AppEvent::ThemeReloaded`] to the event loop, which
//! restyles every window's ImGui, rebuilds the icon atlas if the icon size moved, and repaints.
//! Edit the file, save, look at the window.
//!
//! It follows the same discipline as [`crate::watcher`] (which does this for the *image*), for the
//! same reasons: watch the **directory** rather than the file, because editors save atomically by
//! renaming a temp over the target and a watch on the file itself does not survive that; **debounce**,
//! because one save can arrive as several write bursts; and **never touch the window or the renderer
//! from this thread** — the only thing it does to the UI thread is send an event.
//!
//! A broken stylesheet is not fatal: [`ui::theme::reload`] refuses to install one that doesn't parse
//! or whose colors don't resolve, so the error goes to the console (a debug build keeps its console)
//! and the window keeps drawing with the last good one. Fix the typo, save again.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crossbeam_channel::{select, unbounded, Receiver, Sender};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};

use winit::event_loop::EventLoopProxy;

use crate::app::AppEvent;
use crate::ui::theme;

/// Quiet period after the last filesystem event before we re-read. Long enough to coalesce a
/// multi-burst save (and to let the editor finish writing), short enough to feel instant.
const DEBOUNCE: Duration = Duration::from_millis(120);

/// Handle to the watcher thread, held by the `App` for the window's lifetime. Dropping it closes
/// the channel the thread selects on, which ends the thread and releases the directory watch —
/// the same shutdown discipline as [`crate::watcher::FileWatcher`]. Without it this was the one
/// watcher in the codebase that outlived its window, blocked in `recv()` holding a
/// `ReadDirectoryChangesW` handle on the source tree.
#[derive(Debug)]
pub struct HotStyle {
    _stop: Sender<()>,
}

/// Start watching the stylesheet. Any failure to set the watch up disables hot reload and is
/// otherwise harmless — the app runs on the stylesheet it loaded at startup.
pub fn spawn(proxy: EventLoopProxy<AppEvent>) -> Option<HotStyle> {
    let path = PathBuf::from(theme::SOURCE_PATH);
    if !path.is_file() {
        // A debug build running away from its source tree (someone copied the exe). Nothing to watch.
        return None;
    }
    let (stop_tx, stop_rx) = unbounded::<()>();
    let _ = std::thread::Builder::new()
        .name("fire-theme-watch".into())
        .spawn(move || run(proxy, path, stop_rx));
    Some(HotStyle { _stop: stop_tx })
}

fn run(proxy: EventLoopProxy<AppEvent>, path: PathBuf, stop_rx: Receiver<()>) {
    let Some(dir) = path.parent().map(Path::to_path_buf) else {
        return;
    };
    let name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();

    let (tx, rx) = unbounded::<notify::Result<Event>>();
    let mut watcher: RecommendedWatcher = match notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    }) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("fire: theme hot-reload unavailable: {e}");
            return;
        }
    };
    if let Err(e) = watcher.watch(&dir, RecursiveMode::NonRecursive) {
        eprintln!("fire: cannot watch {}: {e}", dir.display());
        return;
    }
    eprintln!(
        "fire: watching {} — edit it and the window restyles",
        path.display()
    );

    loop {
        // Block until something in the directory changes; ignore its siblings (theme.rs, mod.rs…).
        let received = select! {
            recv(rx) -> ev => ev,
            // The App dropped its guard: the window is going away, and the watch with it.
            recv(stop_rx) -> _ => return,
        };
        match received {
            Ok(Ok(event)) if event.paths.iter().any(|p| p.file_name() == Some(&name)) => {}
            Ok(_) => continue,
            // The watcher died, or the app is going away.
            Err(_) => return,
        }
        // Coalesce the rest of the burst.
        while rx.recv_timeout(DEBOUNCE).is_ok() {}

        match theme::reload() {
            // The UI thread owns every consequence of this (style, icon atlas, clear color, repaint).
            Ok(()) => {
                let _ = proxy.send_event(AppEvent::ThemeReloaded);
            }
            // Keep the last good stylesheet on screen and say why.
            Err(e) => eprintln!("fire: {e}"),
        }
    }
}
