//! Minimal startup config, read once before the window is created. Only the
//! instance-lifecycle mode lives here for now; the full settings surface (Phase 4) extends
//! this struct. Read from `%APPDATA%\fire\config.toml`; a missing/invalid file → defaults.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The commented template written verbatim to `config.toml` on first run (see
/// [`Config::ensure_default_config`]). Kept as a repo file so the docs live in version control and a
/// missing file is a build error — the same posture as the shaders/icons.
const DEFAULT_CONFIG: &str = include_str!("default_config.toml");

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

/// One external application offered in the toolbar's "Open in…" menu (`[[open-with]]` table).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct OpenWithApp {
    /// Label shown in the menu.
    pub name: String,
    /// Full path to the program's executable.
    pub path: String,
    /// Launch arguments; the token `{path}` is replaced with the current image's path. When empty,
    /// the image path is passed as the sole argument.
    #[serde(default)]
    pub args: Vec<String>,
}

impl OpenWithApp {
    /// The argument vector to launch with for `image`: each `args` entry with `{path}` expanded, or
    /// just the image path when `args` is empty.
    pub fn resolved_args(&self, image: &Path) -> Vec<String> {
        let p = image.to_string_lossy();
        if self.args.is_empty() {
            return vec![p.into_owned()];
        }
        self.args.iter().map(|a| a.replace("{path}", &p)).collect()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Config {
    pub instance_mode: InstanceMode,
    /// Reload the displayed image automatically when its file changes on disk. On by default;
    /// set `hot-reload = false` in `config.toml` to disable the file watch entirely.
    pub hot_reload: bool,
    /// User-defined external apps for the toolbar's "Open in…" menu. Empty (button disabled) unless
    /// the user adds `[[open-with]]` blocks.
    pub open_with: Vec<OpenWithApp>,
}

impl Default for Config {
    fn default() -> Self {
        // `#[serde(default)]` fills any missing field from this, so an absent `hot-reload` key
        // (or no config file at all) leaves hot-reload enabled and the open-with list empty.
        Self { instance_mode: InstanceMode::default(), hot_reload: true, open_with: Vec::new() }
    }
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

/// Write the commented [`DEFAULT_CONFIG`] template to `config.toml` *iff* it doesn't already exist,
/// so a fresh install has a discoverable, self-documenting settings file. Best-effort and never
/// destructive: `create_new` atomically fails if the file is present (closing the check-then-write
/// race), so a hand-edited config is never clobbered. A failure here just leaves `load()` on its
/// defaults. Call once at startup, before `load()`.
pub fn ensure_default_config() {
    let Some(path) = config_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
        let _ = f.write_all(DEFAULT_CONFIG.as_bytes());
    }
}

fn config_path() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(PathBuf::from(appdata).join("fire").join("config.toml"))
}
