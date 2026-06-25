//! Minimal startup config, read once before the window is created. Only the
//! instance-lifecycle mode lives here for now; the full settings surface (Phase 4) extends
//! this struct. Read from `%APPDATA%\fire\config.toml`; a missing/invalid file → defaults.

use std::path::PathBuf;

use serde::Deserialize;

/// How a launch relates to any already-running Fire window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum InstanceMode {
    /// Default: each opened image gets its own independent window/process. No mutex, no
    /// pipe — nothing listens in the background.
    #[default]
    NewWindow,
    /// One window; later opens route to it over a named pipe that lives only inside that
    /// visible window's process. (Future: a `compare`/`tabs` single-window mode for
    /// side-by-side viewing — left room in this enum for it.)
    SingleInstance,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, rename_all = "kebab-case")]
pub struct Config {
    pub instance_mode: InstanceMode,
}

impl Config {
    /// Best-effort load; any error (no file, bad TOML) falls back to defaults.
    pub fn load() -> Self {
        config_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }
}

fn config_path() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(PathBuf::from(appdata).join("fire").join("config.toml"))
}
