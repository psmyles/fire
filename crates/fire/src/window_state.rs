//! Persisted window placement. Fire remembers the frame's *restored* (non-maximized)
//! position/size plus whether it was maximized, saved on close and restored on the next
//! launch so the window reopens where the user left it — rather than resizing itself to each
//! image. Stored as `window.toml` in the per-user config directory ([`crate::util::fire_dir`]),
//! separate from the user-edited `config.toml` so writing this runtime state never disturbs
//! hand-authored config.
//!
//! The rectangle is in **physical** pixels: winit's `outer_position` (the frame's top-left, which
//! is what `with_position` takes back) and `inner_size` (the client area, which is what
//! `with_inner_size` takes back). The maximized flag rides alongside because a maximized window's
//! own rect is not the one to restore — the viewer tracks the last *normal* placement and saves
//! that instead.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The saved restored-rect (`x`/`y`/`width`/`height`) and maximized flag.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct WindowState {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub maximized: bool,
}

impl WindowState {
    /// Best-effort load; any error (no file, bad TOML) yields `None` → default placement.
    pub fn load() -> Option<Self> {
        let s = std::fs::read_to_string(state_path()?).ok()?;
        toml::from_str(&s).ok()
    }

    /// Best-effort save; failures are ignored (placement memory is a convenience, not critical).
    pub fn save(&self) {
        let Some(path) = state_path() else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(s) = toml::to_string(self) {
            let _ = std::fs::write(path, s);
        }
    }
}

fn state_path() -> Option<PathBuf> {
    Some(crate::util::fire_dir()?.join("window.toml"))
}
