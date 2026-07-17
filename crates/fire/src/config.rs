//! User settings: the whole of Fire's persisted configuration. Read once at startup (before the
//! window is created) from `%APPDATA%\fire\config.toml`; a missing/invalid file → defaults, so a
//! typo can never stop the app launching.
//!
//! The settings dialog ([`crate::settings`]) edits a *clone* of this struct and hands the edited
//! copy back to the window, which live-applies what it can and calls [`Config::save`]. That makes
//! this the one round-tripping type: every field must both deserialize (hand-edited file) and
//! serialize (dialog write-back). Field *order* matters for `save` — TOML requires plain values
//! before tables, so all scalars are declared ahead of `[flipbook]` / `[context-menu]` /
//! `[keybinds]` / `[[open-with]]`.
//!
//! [`sanitize`](Config::sanitize) is the single clamp chokepoint: it runs after a load and before
//! an apply, so no out-of-range value from a hand edit or a widget ever reaches the renderer.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::flipbook::{FPS_DEFAULT, FPS_MAX, FPS_MIN};
use crate::render::view::{Background, Tonemap};

/// The commented template written verbatim to `config.toml` on first run (see
/// [`Config::ensure_default_config`]). Kept as a repo file so the docs live in version control and a
/// missing file is a build error — the same posture as the shaders/icons.
const DEFAULT_CONFIG: &str = include_str!("default_config.toml");

/// Bounds on the multiplicative zoom step (per wheel notch / key press). Below ~1.01 zooming is
/// imperceptible; above 4× a single notch is a jump.
const ZOOM_STEP_MIN: f32 = 1.01;
const ZOOM_STEP_MAX: f32 = 4.0;
/// Bounds on the exposure step, in stops.
const EXPOSURE_STEP_MIN: f32 = 0.01;
const EXPOSURE_STEP_MAX: f32 = 4.0;

/// How a launch relates to any already-running Fire window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct MenuEntry {
    /// Label shown in the menu.
    pub name: String,
    /// Leaf only: full path to the program's executable. Mutually exclusive with `items`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Leaf only: launch arguments; the token `{path}` is replaced with the current image's path.
    /// When empty, the image path is passed as the sole argument.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Submenu only: nested entries. When non-empty this entry is a submenu (and `path`/`args` are
    /// ignored).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<MenuEntry>,
}

/// The entry at `path` — an index chain from the root (`[1, 0]` = second top-level entry's first
/// child), which is how the actions menu names the item that was clicked.
///
/// Naming it by *path* rather than by a flat command-id index is what makes it impossible for the
/// menu and the launcher to disagree about which app a click meant: there is no second walk of the
/// tree to keep in step with the first.
pub fn entry_at<'a>(entries: &'a [MenuEntry], path: &[usize]) -> Option<&'a MenuEntry> {
    let (&last, parents) = path.split_last()?;
    let mut cur = entries;
    for &i in parents {
        cur = &cur.get(i)?.items;
    }
    cur.get(last)
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

/// The viewport backdrop a freshly opened image gets. `Auto` keeps the built-in per-image rule
/// (real transparency → checkerboard, otherwise black); any other value pins the backdrop for every
/// image, the same override the toolbar's background buttons set at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum BackgroundCfg {
    #[default]
    Auto,
    Black,
    White,
    Grey,
    Checker,
}

impl BackgroundCfg {
    /// The renderer's backdrop override this choice implies — `None` for `Auto` (per-image default).
    pub fn override_for_render(self) -> Option<Background> {
        match self {
            BackgroundCfg::Auto => None,
            BackgroundCfg::Black => Some(Background::Black),
            BackgroundCfg::White => Some(Background::White),
            BackgroundCfg::Grey => Some(Background::Grey),
            BackgroundCfg::Checker => Some(Background::Checker),
        }
    }
}

/// The tonemap operator a freshly adopted HDR image starts on (the `T` key still toggles it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum TonemapCfg {
    #[default]
    Reinhard,
    Aces,
}

impl TonemapCfg {
    pub fn to_render(self) -> Tonemap {
        match self {
            TonemapCfg::Reinhard => Tonemap::Reinhard,
            TonemapCfg::Aces => Tonemap::Aces,
        }
    }
}

/// How an image is scaled when it *opens* (a fresh open, folder navigation, or a re-decode that
/// changed dimensions). The explicit fit command is governed separately by `fit-upscale`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum FitCfg {
    /// Default: shrink an oversized image to fit the window; show a small one at native 1:1.
    #[default]
    Fit,
    /// Always open at native 1:1 (100%), however large the image — pan to explore it.
    ActualSize,
}

/// `[flipbook]` — the defaults a *newly enabled* flipbook adopts, plus the auto-detection gate.
/// Changing these never disturbs a flipbook already set up this session (its per-path state wins);
/// they seed the next one. See [`crate::flipbook`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct FlipbookCfg {
    /// Playback rate a new flipbook starts at, in frames per second.
    #[serde(serialize_with = "serialize_f32")]
    pub fps: f32,
    /// Whether a new flipbook crossfades between frames.
    pub blend: bool,
    /// Whether a new flipbook starts playing immediately (vs. parked on frame 0).
    pub autoplay: bool,
    /// Run sprite-sheet detection on every decoded still image and offer the hint chip when a grid
    /// is found. Off means Fire never scans for sheets — flipbook mode stays available by hand
    /// (`K` / toolbar), it just isn't suggested.
    pub auto_detect: bool,
}

impl Default for FlipbookCfg {
    fn default() -> Self {
        Self {
            fps: FPS_DEFAULT,
            blend: false,
            autoplay: true,
            auto_detect: true,
        }
    }
}

/// `[context-menu]` — visibility of the fixed items in the right-click / actions menu. The
/// configurable "Open in…" entries below them are `[[open-with]]`. Hiding every item here (with no
/// open-with entries) leaves only "Settings…".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct ContextMenuCfg {
    pub show_in_explorer: bool,
    pub copy_file: bool,
    pub copy_path: bool,
    pub copy_file_name: bool,
}

impl Default for ContextMenuCfg {
    fn default() -> Self {
        Self {
            show_in_explorer: true,
            copy_file: true,
            copy_path: true,
            copy_file_name: true,
        }
    }
}

/// `[octagon]` — the octagon overlay (Unity VFX Graph's octagon particle shape, drawn over the
/// image). **Session-only by default**: with `remember = false` the overlay's options reset to the
/// defaults every launch and nothing is written back. `remember = true` persists the options
/// (color / crop factor / hide-outside) on exit and restores them next launch. The overlay's
/// on/off toggle itself always starts off.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct OctagonCfg {
    /// Persist the overlay options across launches (Settings ▸ Overlay).
    pub remember: bool,
    pub color: crate::octagon::LineColor,
    /// 0..1 opacity of the octagon's lines (0 = invisible, 1 = solid).
    #[serde(serialize_with = "serialize_f32")]
    pub line_opacity: f32,
    /// Octagon crop factor: 0 = the full quad, 0.5 = the diamond. Clamped by [`Config::sanitize`].
    #[serde(serialize_with = "serialize_f32")]
    pub crop: f32,
    /// 0..1 fade of the image outside the octagon toward the backdrop.
    #[serde(serialize_with = "serialize_f32")]
    pub hide: f32,
}

impl Default for OctagonCfg {
    fn default() -> Self {
        Self {
            remember: false,
            color: crate::octagon::LineColor::default(),
            line_opacity: 1.0,
            crop: crate::octagon::CROP_DEFAULT,
            hide: 0.0,
        }
    }
}

impl OctagonCfg {
    /// The overlay state a launch starts with: the persisted options when `remember` is on, the
    /// defaults otherwise — and always switched off.
    pub fn initial_state(&self) -> crate::octagon::OctagonState {
        let mut s = crate::octagon::OctagonState::default();
        if self.remember {
            s.color = self.color;
            s.line_opacity = self.line_opacity;
            s.crop = self.crop;
            s.hide = self.hide;
            s.clamp();
        }
        s
    }
}

/// One `[keybinds]` value: a single chord (`fit = "F"`) or several aliases for one action
/// (`zoom-in = ["=", "Num+"]`). Untagged, so TOML's own shape decides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum KeyValue {
    One(String),
    Many(Vec<String>),
}

impl KeyValue {
    /// The chord strings, however they were written. An empty string is an explicit *unbind* and is
    /// preserved as such — [`crate::keybinds`] drops it when parsing, leaving the action with no
    /// chords.
    pub fn as_strings(&self) -> Vec<&str> {
        match self {
            KeyValue::One(s) => vec![s.as_str()],
            KeyValue::Many(v) => v.iter().map(|s| s.as_str()).collect(),
        }
    }
}

/// `[keybinds]` — action name → chord(s). Only bindings the user *changed* are stored; anything
/// absent keeps its shipped default (so future default changes still reach users who never rebound
/// that action). Parsed into the real table by [`crate::keybinds::Keybinds::from_config`], which
/// skips unknown names and unparseable chords rather than failing the load.
pub type KeybindsCfg = std::collections::BTreeMap<String, KeyValue>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Config {
    pub instance_mode: InstanceMode,
    /// Reload the displayed image automatically when its file changes on disk. On by default;
    /// set `hot-reload = false` in `config.toml` to disable the file watch entirely.
    pub hot_reload: bool,
    /// Whether the explicit "fit to window" command (`F` / toolbar) also scales images *smaller*
    /// than the window up to fill it. On by default, so the fit command always fills regardless of
    /// image size; set `fit-upscale = false` to cap it at native 1:1 (the texture-viewer
    /// convention). Note this governs only the explicit command — images always *open* per
    /// `default-fit`, so a small image is shown at 100% on load and folder navigation.
    pub fit_upscale: bool,
    /// Multiplicative zoom per wheel notch / zoom keypress. Clamped to `1.01..=4.0`.
    #[serde(serialize_with = "serialize_f32")]
    pub zoom_step: f32,
    /// Exposure step per `[` / `]` press (HDR sources), in stops. Clamped to `0.01..=4.0`.
    #[serde(serialize_with = "serialize_f32")]
    pub exposure_step: f32,
    /// The tonemap operator a freshly adopted HDR image starts on.
    pub default_tonemap: TonemapCfg,
    /// How an image is scaled when it opens.
    pub default_fit: FitCfg,
    /// The viewport backdrop. `Auto` keeps the per-image default (checker for transparency).
    pub background: BackgroundCfg,
    pub flipbook: FlipbookCfg,
    pub context_menu: ContextMenuCfg,
    /// The octagon overlay's persistence opt-in and (when opted in) its saved options.
    pub octagon: OctagonCfg,
    /// Keyboard bindings the user has changed from the defaults. See [`KeybindsCfg`].
    pub keybinds: KeybindsCfg,
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
            zoom_step: 1.15,
            exposure_step: 0.25,
            default_tonemap: TonemapCfg::default(),
            default_fit: FitCfg::default(),
            background: BackgroundCfg::default(),
            flipbook: FlipbookCfg::default(),
            context_menu: ContextMenuCfg::default(),
            octagon: OctagonCfg::default(),
            keybinds: KeybindsCfg::new(),
            open_with: Vec::new(),
        }
    }
}

impl Config {
    /// Best-effort load; any error (no file, bad TOML) falls back to defaults. Always sanitized, so
    /// a hand-edited out-of-range value can't reach the renderer.
    pub fn load() -> Self {
        let mut cfg: Config = config_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default();
        cfg.sanitize();
        cfg
    }

    /// Clamp every numeric field into range (and replace any NaN with its default). The single
    /// chokepoint for range enforcement: called after a load and before the settings dialog's edits
    /// are applied, so nothing downstream has to re-check.
    pub fn sanitize(&mut self) {
        self.zoom_step = clamp_finite(self.zoom_step, 1.15, ZOOM_STEP_MIN, ZOOM_STEP_MAX);
        self.exposure_step = clamp_finite(
            self.exposure_step,
            0.25,
            EXPOSURE_STEP_MIN,
            EXPOSURE_STEP_MAX,
        );
        self.flipbook.fps = clamp_finite(self.flipbook.fps, FPS_DEFAULT, FPS_MIN, FPS_MAX);
        self.octagon.crop = clamp_finite(
            self.octagon.crop,
            crate::octagon::CROP_DEFAULT,
            crate::octagon::CROP_MIN,
            crate::octagon::CROP_MAX,
        );
        self.octagon.hide = clamp_finite(self.octagon.hide, 0.0, 0.0, 1.0);
        self.octagon.line_opacity = clamp_finite(self.octagon.line_opacity, 1.0, 0.0, 1.0);
    }

    /// Best-effort save to `config.toml`. Written to a sibling temp file and renamed over the
    /// target, so a reader (or a crash mid-write) never sees a truncated config. Failures are
    /// ignored: the settings dialog applies its changes to the live window *before* calling this,
    /// so an unwritable config costs the user persistence, not the edit.
    ///
    /// Note this rewrites the file from the struct — hand-written comments (including the shipped
    /// template's) do not survive the first save. The header below says so in the file itself.
    pub fn save(&self) {
        let Some(path) = config_path() else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let Ok(body) = toml::to_string(self) else {
            return;
        };
        let text = format!("{SAVED_HEADER}{body}");
        // Rename is atomic on the same volume; write the temp file beside the target.
        let tmp = path.with_extension("toml.tmp");
        if std::fs::write(&tmp, text).is_ok() && std::fs::rename(&tmp, &path).is_err() {
            // A failed rename leaves the temp file behind; clear it rather than litter %APPDATA%.
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

/// Prepended to every [`Config::save`], so a user who opens the file after using the settings
/// dialog knows why their comments are gone and that hand-editing still works.
const SAVED_HEADER: &str = "\
# Fire configuration — %APPDATA%\\fire\\config.toml
#
# Written by Fire's Settings dialog. You can still hand-edit this file (it is read at startup, and
# unknown or invalid keys fall back to the built-in defaults), but note that saving from the
# Settings dialog rewrites the file from Fire's own state — comments added here are not preserved.
# Delete the file to regenerate the fully documented template.

";

/// Clamp `v` into `lo..=hi`, substituting `fallback` for a NaN/infinite value.
fn clamp_finite(v: f32, fallback: f32, lo: f32, hi: f32) -> f32 {
    if v.is_finite() {
        v.clamp(lo, hi)
    } else {
        fallback
    }
}

/// Write an `f32` to TOML without the widening artifact.
///
/// TOML floats are `f64`, so serializing `1.15f32` straight through prints its exact binary value —
/// `1.149999976158142`. Nobody wants to open their config and find that. Rounding to four decimals
/// on the way out is lossless for every value these fields can hold (the steppers move in hundredths)
/// and prints as the number the user actually chose.
fn serialize_f32<S: serde::Serializer>(v: &f32, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_f64((*v as f64 * 10_000.0).round() / 10_000.0)
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

        // The actions menu names a clicked entry by its index path, and `entry_at` is what turns that
        // back into the app to launch — so a wrong answer here launches the wrong program.
        assert_eq!(entry_at(&cfg.open_with, &[0]).unwrap().name, "ImageMagick");
        assert_eq!(entry_at(&cfg.open_with, &[0, 0]).unwrap().name, "Convert");
        assert_eq!(entry_at(&cfg.open_with, &[0, 0, 1]).unwrap().name, "PNG");
        // Out of range at any level, and the empty path, are all `None` rather than a panic or a
        // neighbouring entry.
        assert!(entry_at(&cfg.open_with, &[]).is_none());
        assert!(entry_at(&cfg.open_with, &[1]).is_none());
        assert!(entry_at(&cfg.open_with, &[0, 0, 2]).is_none());
        assert!(entry_at(&cfg.open_with, &[0, 9, 0]).is_none());
    }

    /// The shipped default template is valid TOML and deserializes (its `[[open-with]]` examples are
    /// all commented out, so it yields an empty menu).
    #[test]
    fn default_template_parses() {
        let cfg: Config = toml::from_str(DEFAULT_CONFIG).unwrap();
        assert!(cfg.open_with.is_empty());
        assert_eq!(cfg, Config::default());
    }

    /// A config written by the settings dialog reads back identical — the round-trip the dialog's
    /// save/load cycle depends on. Also pins the TOML value-before-table field order: `to_string`
    /// errors out if a scalar is declared after `[flipbook]` / `[[open-with]]`.
    #[test]
    fn save_round_trip() {
        let cfg = Config {
            instance_mode: InstanceMode::SingleInstance,
            hot_reload: false,
            fit_upscale: false,
            zoom_step: 1.25,
            exposure_step: 0.5,
            default_tonemap: TonemapCfg::Aces,
            default_fit: FitCfg::ActualSize,
            background: BackgroundCfg::Grey,
            flipbook: FlipbookCfg {
                fps: 12.0,
                blend: true,
                autoplay: false,
                auto_detect: false,
            },
            context_menu: ContextMenuCfg {
                show_in_explorer: false,
                copy_file: true,
                copy_path: false,
                copy_file_name: true,
            },
            octagon: OctagonCfg {
                remember: true,
                color: crate::octagon::LineColor::Blue,
                line_opacity: 0.5,
                crop: 0.3,
                hide: 0.75,
            },
            keybinds: [
                ("fit".to_string(), KeyValue::One("Ctrl+F".into())),
                (
                    "zoom-in".to_string(),
                    KeyValue::Many(vec!["=".into(), "Num+".into()]),
                ),
            ]
            .into_iter()
            .collect(),
            open_with: vec![MenuEntry {
                name: "Tools".into(),
                path: None,
                args: vec![],
                items: vec![MenuEntry {
                    name: "Photoshop".into(),
                    path: Some(r"C:\ps.exe".into()),
                    args: vec!["{path}".into()],
                    items: vec![],
                }],
            }],
        };
        let text = toml::to_string(&cfg).expect("serializes");
        let back: Config = toml::from_str(&text).expect("deserializes");
        assert_eq!(back, cfg);
    }

    /// A saved config reads like a human wrote it: `1.15`, not `1.149999976158142` (what an f32
    /// widened to a TOML f64 prints as).
    #[test]
    fn floats_serialize_without_the_widening_artifact() {
        let text = toml::to_string(&Config::default()).unwrap();
        assert!(
            text.contains("zoom-step = 1.15"),
            "expected a clean float, got:\n{text}"
        );
        assert!(text.contains("exposure-step = 0.25"));
        assert!(text.contains("fps = 24.0"));
        // …and it still round-trips exactly.
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back, Config::default());
    }

    /// The header `save` prepends is a comment block, so a saved file still parses.
    #[test]
    fn saved_header_is_valid_toml() {
        let body = toml::to_string(&Config::default()).unwrap();
        let cfg: Config = toml::from_str(&format!("{SAVED_HEADER}{body}")).unwrap();
        assert_eq!(cfg, Config::default());
    }

    /// A pre-settings-dialog config (only the original four keys) still parses, with every new key
    /// taking its default — the backward-compat guarantee for existing installs.
    #[test]
    fn legacy_config_parses_with_new_defaults() {
        let cfg: Config = toml::from_str(
            r#"
            instance-mode = "single-instance"
            hot-reload = false
            fit-upscale = false
            "#,
        )
        .unwrap();
        assert_eq!(cfg.instance_mode, InstanceMode::SingleInstance);
        assert!(!cfg.hot_reload);
        assert!(!cfg.fit_upscale);
        // Everything the settings dialog added is defaulted.
        assert_eq!(cfg.zoom_step, 1.15);
        assert_eq!(cfg.background, BackgroundCfg::Auto);
        assert_eq!(cfg.flipbook, FlipbookCfg::default());
        assert_eq!(cfg.context_menu, ContextMenuCfg::default());
    }

    /// Out-of-range and non-finite numbers from a hand edit are clamped, never propagated.
    #[test]
    fn sanitize_clamps_numbers() {
        let mut cfg = Config {
            zoom_step: 100.0,
            exposure_step: 0.0,
            flipbook: FlipbookCfg {
                fps: 1e9,
                ..FlipbookCfg::default()
            },
            ..Config::default()
        };
        cfg.sanitize();
        assert_eq!(cfg.zoom_step, ZOOM_STEP_MAX);
        assert_eq!(cfg.exposure_step, EXPOSURE_STEP_MIN);
        assert_eq!(cfg.flipbook.fps, FPS_MAX);

        let mut nan = Config {
            zoom_step: f32::NAN,
            exposure_step: f32::INFINITY,
            ..Config::default()
        };
        nan.sanitize();
        assert_eq!(nan.zoom_step, 1.15);
        assert_eq!(nan.exposure_step, 0.25);
    }
}
