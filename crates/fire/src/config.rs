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

/// One entry in the toolbar's "Open in…" menu tree (`[[open-with]]`). An entry is either a **leaf**
/// that launches an external program (`path` set) or a **submenu** that nests further entries
/// (`items` non-empty), and submenus can nest to any depth — `[[open-with.items]]`,
/// `[[open-with.items.items]]`, and so on. If an entry has both `items` and `path`, the submenu
/// wins: it is shown as a submenu and `path`/`args` are ignored. An entry with neither is malformed
/// and skipped.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct MenuEntry {
    /// Label shown in the menu.
    pub name: String,
    /// Leaf only: full path to the program's executable. Mutually exclusive with `items`.
    #[serde(default)]
    pub path: Option<String>,
    /// Leaf only: launch arguments; the token `{path}` is replaced with the current image's path.
    /// When empty, the image path is passed as the sole argument.
    #[serde(default)]
    pub args: Vec<String>,
    /// Submenu only: nested entries. When non-empty this entry is a submenu (and `path`/`args` are
    /// ignored).
    #[serde(default)]
    pub items: Vec<MenuEntry>,
}

impl MenuEntry {
    /// Whether this entry is a submenu (has children). A submenu's `path`/`args` are ignored.
    pub fn is_submenu(&self) -> bool {
        !self.items.is_empty()
    }

    /// The argument vector to launch a leaf entry with for `image`: each `args` entry with `{path}`
    /// expanded, or just the image path when `args` is empty.
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
    /// Whether the explicit "fit to window" command (`F` / toolbar) also scales images *smaller*
    /// than the window up to fill it. On by default, so the fit command always fills regardless of
    /// image size; set `fit-upscale = false` to cap it at native 1:1 (the texture-viewer
    /// convention). Note this governs only the explicit command — images always *open* fitted
    /// without upscaling, so a small image is shown at 100% on load and folder navigation.
    pub fit_upscale: bool,
    /// User-defined entries for the toolbar's "Open in…" menu — external apps and/or nested
    /// submenus. Empty (button disabled) unless the user adds `[[open-with]]` blocks.
    pub open_with: Vec<MenuEntry>,
}

impl Default for Config {
    fn default() -> Self {
        // `#[serde(default)]` fills any missing field from this, so an absent `hot-reload` key
        // (or no config file at all) leaves hot-reload enabled, fit-upscale enabled, and the
        // open-with list empty.
        Self {
            instance_mode: InstanceMode::default(),
            hot_reload: true,
            fit_upscale: true,
            open_with: Vec::new(),
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A plain leaf `[[open-with]]` block (the pre-submenu format) still parses: a leaf is just an
    /// entry with a `path` and no `items`.
    #[test]
    fn open_with_leaf_back_compat() {
        let cfg: Config = toml::from_str(
            r#"
            [[open-with]]
            name = "Photoshop"
            path = 'C:\ps.exe'
            args = ["{path}"]
            "#,
        )
        .unwrap();
        assert_eq!(cfg.open_with.len(), 1);
        let e = &cfg.open_with[0];
        assert!(!e.is_submenu());
        assert_eq!(e.path.as_deref(), Some(r"C:\ps.exe"));
        assert_eq!(e.resolved_args(Path::new(r"D:\img.png")), vec![r"D:\img.png"]);
    }

    /// Nested `[[open-with.items]]` / `[[open-with.items.items]]` parses into the submenu tree, and
    /// `is_submenu()` distinguishes branches from leaves at every level.
    #[test]
    fn open_with_nested_submenus() {
        let cfg: Config = toml::from_str(
            r#"
            [[open-with]]
            name = "ImageMagick"
              [[open-with.items]]
              name = "Convert"
                [[open-with.items.items]]
                name = "JPG"
                path = 'C:\magick.exe'
                args = ["{path}", "{path}.jpg"]
                [[open-with.items.items]]
                name = "PNG"
                path = 'C:\magick.exe'
            "#,
        )
        .unwrap();
        assert_eq!(cfg.open_with.len(), 1);
        let im = &cfg.open_with[0];
        assert!(im.is_submenu());
        assert_eq!(im.items.len(), 1);

        let convert = &im.items[0];
        assert!(convert.is_submenu());
        assert_eq!(convert.items.len(), 2);

        let jpg = &convert.items[0];
        assert!(!jpg.is_submenu());
        assert_eq!(
            jpg.resolved_args(Path::new(r"D:\a.tga")),
            vec![r"D:\a.tga", r"D:\a.tga.jpg"]
        );
        // A leaf with no `args` falls back to passing the image path alone.
        assert_eq!(convert.items[1].resolved_args(Path::new(r"D:\a.tga")), vec![r"D:\a.tga"]);
    }

    /// The shipped default template is valid TOML and deserializes (its `[[open-with]]` examples are
    /// all commented out, so it yields an empty menu).
    #[test]
    fn default_template_parses() {
        let cfg: Config = toml::from_str(DEFAULT_CONFIG).unwrap();
        assert!(cfg.open_with.is_empty());
    }
}
