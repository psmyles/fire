//! Explorer thumbnails — everything about the feature except the COM object itself.
//!
//! Windows asks a registered `IThumbnailProvider` for a format's thumbnails; Fire's lives in
//! `fire-shellext`, a DLL Explorer loads into its COM surrogate (`dllhost.exe`), never into
//! Explorer itself. That DLL is deliberately thin: it reads the file Explorer hands it, decodes it
//! with `fire-decode`, and turns the result into a bitmap with [`render`]. What is left over lives
//! here, because `fire.exe` needs it too:
//!
//! * [`settings`] — the `[thumbnails]` table of `config.toml`. The Settings dialog edits it; the DLL
//!   reads it fresh for every thumbnail, so a change applies to the next one Explorer asks for.
//! * [`formats`] — which extensions make up each checkbox in the Settings tab and the installer.
//! * [`registry`] (Windows) — turning formats on and off in `HKCU`, and putting back whatever
//!   provider Fire displaced. The installer drives it through `fire.exe --set-thumbnails`, the
//!   Settings tab directly, so there is one implementation of it.
//! * [`cache`] (Windows) — emptying Explorer's thumbnail cache, the only way a changed setting
//!   reaches thumbnails Explorer has already made.
//!
//! macOS has nothing to register — Finder's Quick Look covers the formats — so only the portable
//! half builds there. The settings table still parses, so one `config.toml` serves both.

pub mod formats;
pub mod render;
pub mod settings;

#[cfg(windows)]
pub mod cache;
#[cfg(windows)]
pub mod registry;

pub use settings::{Backdrop, ThumbnailsCfg, Tonemap};

/// The class id of Fire's thumbnail provider: what the per-extension `ShellEx` entries name, what
/// `HKCU\Software\Classes\CLSID` maps to the DLL, and what the DLL's `DllGetClassObject` answers
/// to. Never change it once shipped — the registrations already out there name this one.
///
/// `installer/fire.iss` spells it out too (it reads the registry to show the current state on an
/// upgrade); a test keeps the two equal.
pub const CLSID: u128 = 0xDA926B60_8532_4616_A98E_889545ADA4AB;

/// [`CLSID`] in the braced registry form, `{DA926B60-8532-4616-A98E-889545ADA4AB}`.
pub fn clsid_braced() -> String {
    let v = CLSID;
    format!(
        "{{{:08X}-{:04X}-{:04X}-{:04X}-{:012X}}}",
        (v >> 96) as u32,
        (v >> 80) as u16,
        (v >> 64) as u16,
        (v >> 48) as u16,
        v & 0xFFFF_FFFF_FFFF
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clsid_formats_as_the_registry_spells_it() {
        assert_eq!(clsid_braced(), "{DA926B60-8532-4616-A98E-889545ADA4AB}");
    }

    /// The installer reads the registry for this class id to tick the formats already on, so its
    /// copy has to be this one. Lines of the form `FireThumbClsid = '{…}';`.
    #[test]
    fn installer_names_the_same_clsid() {
        const ISS: &str = include_str!("../../../installer/fire.iss");
        let line = ISS
            .lines()
            .find(|l| l.trim_start().starts_with("FireThumbClsid"))
            .expect("fire.iss declares FireThumbClsid");
        assert!(
            line.contains(&clsid_braced()),
            "fire.iss names a different class id: {line}"
        );
    }
}
