//! The formats Fire can draw Explorer thumbnails for, grouped the way the user switches them: one
//! checkbox per format, covering all of that format's extensions.
//!
//! The groups and their ids are the installer's — `assoc\png` and `thumbs…\png` there are `png`
//! here — so the two pages of the installer, the Settings tab and `--set-thumbnails` all speak of
//! the same things. Tests hold this table to `fire-decode`'s extension list and to the installer.

/// One checkbox: a format and every extension it is known by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatGroup {
    /// The stable name — the installer's task name and the `--set-thumbnails` token. Never rename
    /// one: an installed Fire's upgrade reads them back.
    pub id: &'static str,
    /// The checkbox text.
    pub label: &'static str,
    /// Every extension, lower-case, no dot.
    pub exts: &'static [&'static str],
    /// Whether Fire provides this format's thumbnails out of the box. On exactly where Windows
    /// draws none of its own — or, for DDS, one that only reads the oldest block formats (BC1-3),
    /// which is not what a game pipeline writes. Everything Windows' photo thumbnailer already
    /// covers starts off: taking it over is the user's call, not the installer's.
    pub default_on: bool,
}

impl FormatGroup {
    /// The extension the installer probes to show whether the group is on.
    pub fn primary_ext(&self) -> &'static str {
        self.exts[0]
    }
}

/// Every group, the ones Fire takes by default first — the order both the Settings tab and the
/// installer list them in.
pub const GROUPS: &[FormatGroup] = &[
    // --- formats Windows draws no thumbnail for -------------------------------------------------
    FormatGroup {
        id: "tga",
        label: "Truevision TGA",
        exts: &["tga"],
        default_on: true,
    },
    FormatGroup {
        id: "dds",
        label: "DirectDraw Surface",
        exts: &["dds"],
        default_on: true,
    },
    FormatGroup {
        id: "psd",
        label: "Photoshop",
        exts: &["psd", "psb"],
        default_on: true,
    },
    FormatGroup {
        id: "exr",
        label: "OpenEXR",
        exts: &["exr"],
        default_on: true,
    },
    FormatGroup {
        id: "hdr",
        label: "Radiance HDR",
        exts: &["hdr", "pic", "rgbe", "xyze"],
        default_on: true,
    },
    FormatGroup {
        id: "qoi",
        label: "QOI",
        exts: &["qoi"],
        default_on: true,
    },
    FormatGroup {
        id: "netpbm",
        label: "Netpbm",
        exts: &["ppm", "pgm", "pbm", "pnm", "pam"],
        default_on: true,
    },
    FormatGroup {
        id: "farbfeld",
        label: "Farbfeld",
        exts: &["ff"],
        default_on: true,
    },
    // --- formats Windows already thumbnails -----------------------------------------------------
    FormatGroup {
        id: "png",
        label: "PNG",
        exts: &["png"],
        default_on: false,
    },
    FormatGroup {
        id: "jpeg",
        label: "JPEG",
        exts: &["jpg", "jpeg", "jpe", "jfif"],
        default_on: false,
    },
    FormatGroup {
        id: "gif",
        label: "GIF",
        exts: &["gif"],
        default_on: false,
    },
    FormatGroup {
        id: "bmp",
        label: "Bitmap",
        exts: &["bmp", "dib"],
        default_on: false,
    },
    FormatGroup {
        id: "tiff",
        label: "TIFF",
        exts: &["tif", "tiff", "tx"],
        default_on: false,
    },
    FormatGroup {
        id: "webp",
        label: "WebP",
        exts: &["webp"],
        default_on: false,
    },
    FormatGroup {
        id: "ico",
        label: "Icon and cursor",
        exts: &["ico", "cur"],
        default_on: false,
    },
    FormatGroup {
        id: "jxl",
        label: "JPEG XL",
        exts: &["jxl"],
        default_on: false,
    },
    FormatGroup {
        id: "heif",
        label: "HEIF / HEIC",
        exts: &["heic", "heif"],
        default_on: false,
    },
    FormatGroup {
        id: "avif",
        label: "AVIF",
        exts: &["avif"],
        default_on: false,
    },
    FormatGroup {
        id: "raw",
        label: "Camera raw",
        exts: &[
            "cr2", "cr3", "crw", "nef", "nrw", "arw", "srf", "sr2", "raf", "orf", "rw2", "pef",
            "srw", "dng", "x3f", "3fr", "fff", "iiq", "erf", "mrw", "dcr", "kdc", "mef", "mos",
            "rwl", "gpr", "raw",
        ],
        default_on: false,
    },
];

/// The group with this id.
pub fn group(id: &str) -> Option<&'static FormatGroup> {
    GROUPS.iter().find(|g| g.id == id)
}

/// The groups on out of the box, by id.
pub fn default_ids() -> Vec<&'static str> {
    GROUPS
        .iter()
        .filter(|g| g.default_on)
        .map(|g| g.id)
        .collect()
}

/// Read a `--set-thumbnails` list (`"tga,psd,exr"`; commas and/or spaces). Unknown ids are
/// returned separately rather than dropped, so the caller can refuse a typo instead of silently
/// switching that format off.
pub fn parse_ids(s: &str) -> (Vec<&'static str>, Vec<String>) {
    let mut known = Vec::new();
    let mut unknown = Vec::new();
    for tok in s.split([',', ' ', ';']).filter(|t| !t.is_empty()) {
        match group(&tok.to_ascii_lowercase()) {
            Some(g) if !known.contains(&g.id) => known.push(g.id),
            Some(_) => {}
            None => unknown.push(tok.to_string()),
        }
    }
    (known, unknown)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// Every extension Fire opens belongs to exactly one checkbox, and no checkbox claims one Fire
    /// cannot decode — otherwise a format would be impossible to switch on, or Explorer would be
    /// told to ask Fire for a file it then refuses.
    #[test]
    fn groups_cover_the_extension_table_exactly_once() {
        let mut seen = BTreeSet::new();
        for g in GROUPS {
            assert!(!g.exts.is_empty(), "{} has no extensions", g.id);
            for e in g.exts {
                assert!(seen.insert(*e), ".{e} is in two groups");
            }
        }
        let decoder: BTreeSet<&str> = fire_decode::SUPPORTED_EXTENSIONS.iter().copied().collect();
        let missing: Vec<_> = decoder.difference(&seen).collect();
        let extra: Vec<_> = seen.difference(&decoder).collect();
        assert!(
            missing.is_empty() && extra.is_empty(),
            "formats table and SUPPORTED_EXTENSIONS disagree.\n  no checkbox: {missing:?}\n  \
             not decodable: {extra:?}"
        );
    }

    #[test]
    fn ids_are_unique_and_lower_case() {
        let mut seen = BTreeSet::new();
        for g in GROUPS {
            assert!(seen.insert(g.id), "{} listed twice", g.id);
            assert_eq!(g.id, g.id.to_ascii_lowercase());
        }
    }

    /// The default-on groups come first, so both lists can split into "Windows has none" and
    /// "Windows has its own" under one heading each.
    #[test]
    fn default_on_groups_lead() {
        let first_off = GROUPS.iter().position(|g| !g.default_on).unwrap();
        assert!(GROUPS[first_off..].iter().all(|g| !g.default_on));
    }

    #[test]
    fn parse_ids_keeps_order_drops_duplicates_and_reports_typos() {
        let (known, unknown) = parse_ids("tga, PSD;exr tga  pngg");
        assert_eq!(known, ["tga", "psd", "exr"]);
        assert_eq!(unknown, ["pngg"]);
        assert_eq!(parse_ids(""), (vec![], vec![]));
    }

    /// The installer lists one task per group under two parents, `thumbs\new\<id>` (on by default)
    /// and `thumbs\win\<id>` (off). Read it back and compare outright, the way `fire-decode` polices
    /// its association rows — an Inno script cannot `use` this table.
    #[test]
    fn installer_thumbnail_tasks_match_the_table() {
        const ISS: &str = include_str!("../../../installer/fire.iss");
        let mut tasks: Vec<(String, bool)> = Vec::new();
        for line in ISS.lines() {
            let Some(rest) = line.strip_prefix(r#"Name: "thumbs\"#) else {
                continue;
            };
            let Some(name) = rest.split('"').next() else {
                continue;
            };
            match name.split_once('\\') {
                Some(("new", id)) => tasks.push((id.to_string(), true)),
                Some(("win", id)) => tasks.push((id.to_string(), false)),
                _ => {}
            }
        }
        let table: Vec<(String, bool)> = GROUPS
            .iter()
            .map(|g| (g.id.to_string(), g.default_on))
            .collect();
        assert_eq!(
            tasks, table,
            "installer/fire.iss's thumbnail tasks differ from formats::GROUPS (ids, order, or which \
             parent — `new` is on by default, `win` off)"
        );

        // …and its [Code] list, which ticks the tasks on an upgrade by probing each format's
        // primary extension: lines of the form `Result.Add('new\tga=tga');`.
        let code: Vec<String> = ISS
            .lines()
            .filter_map(|l| l.trim().strip_prefix("Result.Add('"))
            .filter_map(|l| l.split('\'').next())
            .map(str::to_string)
            .collect();
        let expected: Vec<String> = GROUPS
            .iter()
            .map(|g| {
                let parent = if g.default_on { "new" } else { "win" };
                format!(r"{parent}\{}={}", g.id, g.primary_ext())
            })
            .collect();
        assert_eq!(
            code, expected,
            "installer/fire.iss's ThumbFormatList differs from formats::GROUPS"
        );
    }
}
