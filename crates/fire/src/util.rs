//! Small helpers shared across modules.

/// The per-user directory every persisted file lives in (`config.toml`, `window.toml`):
/// `%APPDATA%\fire` on Windows, `~/Library/Application Support/fire` on macOS,
/// `$XDG_CONFIG_HOME/fire` elsewhere. One definition, so the files cannot drift into different
/// directories.
pub fn fire_dir() -> Option<std::path::PathBuf> {
    Some(dirs::config_dir()?.join("fire"))
}
