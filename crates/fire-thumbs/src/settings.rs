//! `[thumbnails]` — how Fire draws the thumbnails Explorer asks it for.
//!
//! One table in Fire's own `config.toml`, so there is a single settings file: the viewer's Settings
//! dialog edits it through `fire`'s `Config` (which embeds this struct), and the DLL reads just
//! this table back with [`ThumbnailsCfg::load`], ignoring every other key.
//!
//! Which *formats* get Fire thumbnails is not here: that is registry state, the thing Explorer
//! actually reads, and keeping a second copy in the config would only give the two a way to
//! disagree. See [`crate::registry`].

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The exposure range, in stops either way — the viewer's own exposure slider's.
pub const EXPOSURE_MAX: f32 = 8.0;

/// The largest value `max-file-size-mb` takes, in MB. Past this a limit is not limiting anything.
pub const MAX_FILE_SIZE_MB_MAX: u32 = 16 * 1024;

/// What shows through a thumbnail's transparent pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Backdrop {
    /// Real transparency: Explorer's own background shows through, as it does for its own PNG
    /// thumbnails.
    #[default]
    Transparent,
    /// The viewer's light/dark checkerboard.
    Checker,
    Black,
    White,
    /// The viewer's neutral 40% grey.
    Grey,
}

/// The operator HDR thumbnails (OpenEXR, Radiance HDR, float DDS) are tonemapped with — the
/// viewer's two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Tonemap {
    #[default]
    Reinhard,
    Aces,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct ThumbnailsCfg {
    pub backdrop: Backdrop,
    pub tonemap: Tonemap,
    /// Exposure applied to HDR sources before tonemapping, in stops. LDR sources ignore it, as
    /// they do in the viewer.
    #[serde(serialize_with = "serialize_f32")]
    pub exposure: f32,
    /// Files larger than this many MB get no thumbnail from Fire — Explorer shows the file's icon.
    /// `0` is no limit.
    ///
    /// The decode needs the whole file in memory and then the whole decoded image, so a 2 GB
    /// Photoshop document costs gigabytes for a 256-pixel picture. This is where that stops.
    pub max_file_size_mb: u32,
}

impl Default for ThumbnailsCfg {
    fn default() -> Self {
        Self {
            backdrop: Backdrop::default(),
            tonemap: Tonemap::default(),
            exposure: 0.0,
            max_file_size_mb: 1024,
        }
    }
}

impl ThumbnailsCfg {
    /// Clamp the numbers into range (a NaN exposure becomes 0). The same chokepoint role
    /// `fire`'s `Config::sanitize` plays, which calls this.
    pub fn sanitize(&mut self) {
        self.exposure = if self.exposure.is_finite() {
            self.exposure.clamp(-EXPOSURE_MAX, EXPOSURE_MAX)
        } else {
            0.0
        };
        self.max_file_size_mb = self.max_file_size_mb.min(MAX_FILE_SIZE_MB_MAX);
    }

    /// The size limit in bytes, `None` for no limit.
    pub fn max_file_bytes(&self) -> Option<u64> {
        (self.max_file_size_mb > 0).then(|| u64::from(self.max_file_size_mb) << 20)
    }

    /// Read `[thumbnails]` out of `config.toml`, sanitized. A missing file, a missing table or a
    /// file that is not TOML at all gives the defaults — the viewer's posture: a typo must never
    /// cost the user their thumbnails.
    pub fn load() -> Self {
        Self::parse(
            &config_path()
                .and_then(|p| std::fs::read_to_string(p).ok())
                .unwrap_or_default(),
        )
    }

    /// [`load`](Self::load) on text already read.
    pub fn parse(text: &str) -> Self {
        /// Just the one table. serde skips every key it does not name, so the rest of the
        /// viewer's config — keybinds, menus — never has to parse for thumbnails to work.
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct File {
            thumbnails: ThumbnailsCfg,
        }
        let mut cfg = toml::from_str::<File>(text)
            .map(|f| f.thumbnails)
            .unwrap_or_default();
        cfg.sanitize();
        cfg
    }
}

/// `config.toml`'s path — the file `fire`'s `util::fire_dir` names (`%APPDATA%\fire` on Windows).
/// The DLL runs in a COM surrogate under the user's own account, so `%APPDATA%` is the same
/// folder there.
fn config_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("fire").join("config.toml"))
}

/// Write an `f32` without the f32→f64 widening artifact (`0.3` rather than `0.30000001192092896`) —
/// the same fix `fire`'s config applies to its own floats.
fn serialize_f32<S: serde::Serializer>(v: &f32, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_f64((*v as f64 * 10_000.0).round() / 10_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_its_table_out_of_a_full_config() {
        let cfg = ThumbnailsCfg::parse(
            r#"
            open-in = "reuse-window"
            zoom-step = 1.2

            [flipbook]
            fps = 12.0

            [thumbnails]
            backdrop = "checker"
            tonemap = "aces"
            exposure = -1.5
            max-file-size-mb = 256

            [[open-with]]
            name = "x"
            path = 'C:\x.exe'
            "#,
        );
        assert_eq!(
            cfg,
            ThumbnailsCfg {
                backdrop: Backdrop::Checker,
                tonemap: Tonemap::Aces,
                exposure: -1.5,
                max_file_size_mb: 256,
            }
        );
        assert_eq!(cfg.max_file_bytes(), Some(256 << 20));
    }

    /// No table, an empty file, or a file that is not TOML: the defaults, never a failure.
    #[test]
    fn anything_unreadable_is_the_defaults() {
        for text in ["", "hot-reload = false", "this is [[[ not toml"] {
            assert_eq!(ThumbnailsCfg::parse(text), ThumbnailsCfg::default());
        }
    }

    #[test]
    fn sanitize_clamps() {
        let mut c = ThumbnailsCfg {
            exposure: 100.0,
            max_file_size_mb: u32::MAX,
            ..Default::default()
        };
        c.sanitize();
        assert_eq!(c.exposure, EXPOSURE_MAX);
        assert_eq!(c.max_file_size_mb, MAX_FILE_SIZE_MB_MAX);
        c.exposure = f32::NAN;
        c.sanitize();
        assert_eq!(c.exposure, 0.0);
        c.max_file_size_mb = 0;
        assert_eq!(c.max_file_bytes(), None);
    }

    #[test]
    fn round_trips_with_clean_floats() {
        #[derive(Serialize, Deserialize)]
        struct File {
            thumbnails: ThumbnailsCfg,
        }
        let cfg = ThumbnailsCfg {
            exposure: 0.3,
            ..Default::default()
        };
        let text = toml::to_string(&File { thumbnails: cfg }).unwrap();
        assert!(text.contains("exposure = 0.3\n"), "{text}");
        assert_eq!(ThumbnailsCfg::parse(&text), cfg);
    }
}
