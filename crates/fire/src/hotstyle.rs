//! Live-reload of the stylesheet — **debug builds only** (`main.rs` doesn't declare this module in
//! release, so none of it is compiled in).
//!
//! A thread watches `crates/fire/src/ui/theme.toml` in the *source tree* (its path is baked in at
//! compile time by [`crate::ui::theme::SOURCE_PATH`]). On a change it re-parses the stylesheet,
//! installs it if it is valid, and posts [`crate::win::WM_APP_THEME_RELOADED`] to the window, which
//! restyles ImGui, rebuilds the icon atlas if the icon size moved, and repaints. Edit the file, save,
//! look at the window.
//!
//! It follows the same discipline as [`crate::watcher`] (which does this for the *image*), for the
//! same reasons: watch the **directory** rather than the file, because editors save atomically by
//! renaming a temp over the target and a watch on the file itself does not survive that; **debounce**,
//! because one save can arrive as several write bursts; and **never touch the window or the renderer
//! from this thread** — the only thing it does to the UI thread is `PostMessage`.
//!
//! A broken stylesheet is not fatal: [`ui::theme::reload`] refuses to install one that doesn't parse
//! or whose colors don't resolve, so the error goes to the console (a debug build keeps its console)
//! and the window keeps drawing with the last good one. Fix the typo, save again.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crossbeam_channel::unbounded;
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::ui::theme;
use crate::win::WM_APP_THEME_RELOADED;

/// Quiet period after the last filesystem event before we re-read. Long enough to coalesce a
/// multi-burst save (and to let the editor finish writing), short enough to feel instant.
const DEBOUNCE: Duration = Duration::from_millis(120);

/// Start watching the stylesheet. `frame` is the window's HWND (as `isize`, so it crosses the thread
/// boundary like the decode pool's). Any failure to set the watch up disables hot reload and is
/// otherwise harmless — the app runs on the stylesheet it loaded at startup.
pub fn spawn(frame: isize) {
    let path = PathBuf::from(theme::SOURCE_PATH);
    if !path.is_file() {
        // A debug build running away from its source tree (someone copied the exe). Nothing to watch.
        return;
    }
    let _ = std::thread::Builder::new()
        .name("fire-theme-watch".into())
        .spawn(move || run(frame, path));
}

fn run(frame: isize, path: PathBuf) {
    let Some(dir) = path.parent().map(Path::to_path_buf) else {
        return;
    };
    let name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();

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
    eprintln!("fire: watching {} — edit it and the window restyles", path.display());

    loop {
        // Block until something in the directory changes; ignore its siblings (theme.rs, mod.rs…).
        match rx.recv() {
            Ok(Ok(event)) if event.paths.iter().any(|p| p.file_name() == Some(&name)) => {}
            Ok(_) => continue,
            // The watcher died, or the app is going away.
            Err(_) => return,
        }
        // Coalesce the rest of the burst.
        while rx.recv_timeout(DEBOUNCE).is_ok() {}

        match theme::reload() {
            // The UI thread owns every consequence of this (style, icon atlas, clear color, repaint).
            Ok(()) => unsafe {
                PostMessageW(frame as HWND, WM_APP_THEME_RELOADED, 0, 0);
            },
            // Keep the last good stylesheet on screen and say why.
            Err(e) => eprintln!("fire: {e}"),
        }
    }
}
