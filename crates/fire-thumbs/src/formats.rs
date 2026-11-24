//! The formats Fire can draw Explorer thumbnails for, grouped the way the user switches them: one
//! checkbox per format, covering all of that format's extensions.
//!
//! The groups and their ids are the installer's — its `assoc\png` task and the `'png'` row of its
//! thumbnails page are `png` here — so the two pages of the installer, the Settings tab and
//! `--set-thumbnails` all speak of the same things. Tests hold this table to `fire-decode`'s extension list and to the installer.

/// One checkbox: a format and every extension it is known by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatGroup {
    /// The stable name: the `--set-thumbnails` token, which the installer passes for the boxes
    /// ticked on its thumbnails page. Never rename one — scripts that pass the list would break.
    pub id: &'static str,
    /// The checkbox text.
    pub label: &'static str,
    /// Every extension, lower-case, no dot.
    pub exts: &'static [&'static str],
    /// Whether Windows draws no thumbnail of its own — or, for DDS, one that only reads the
    /// oldest block formats (BC1-3), which is not what a game pipeline writes. Only which column
    /// the format is listed in, in Settings ▸ Thumbnails and on the installer's page: the
    /// installer ticks every format either way.
    pub windows_has_none: bool,
}

/// Every group, the ones Windows draws no thumbnail for first — the order both the Settings tab
/// and the installer list them in.
pub const GROUPS: &[FormatGroup] = &[
    // --- formats Windows draws no thumbnail for -------------------------------------------------
    FormatGroup {
        id: "tga",
        label: "Truevision TGA",
        exts: &["tga"],
        windows_has_none: true,
    },
    FormatGroup {
        id: "dds",
        label: "DirectDraw Surface",
        exts: &["dds"],
        windows_has_none: true,
    },
    FormatGroup {
        id: "ktx",
        label: "Khronos texture",
        exts: &["ktx", "ktx2"],
        windows_has_none: true,
    },
    FormatGroup {
        id: "psd",
        label: "Photoshop",
        exts: &["psd", "psb"],
        windows_has_none: true,
    },
    FormatGroup {
        id: "exr",
        label: "OpenEXR",
        exts: &["exr"],
        windows_has_none: true,
    },
    FormatGroup {
        id: "hdr",
        label: "Radiance HDR",
        exts: &["hdr", "pic", "rgbe", "xyze"],
        windows_has_none: true,
    },
    FormatGroup {
        id: "qoi",
        label: "QOI",
        exts: &["qoi"],
        windows_has_none: true,
    },
    FormatGroup {
        id: "netpbm",
        label: "Netpbm",
        exts: &["ppm", "pgm", "pbm", "pnm", "pam"],
        windows_has_none: true,
    },
    FormatGroup {
        id: "farbfeld",
        label: "Farbfeld",
        exts: &["ff"],
        windows_has_none: true,
    },
    // --- formats Windows already thumbnails -----------------------------------------------------
    FormatGroup {
        id: "png",
        label: "PNG",
        exts: &["png"],
        windows_has_none: false,
    },
    FormatGroup {
        id: "jpeg",
        label: "JPEG",
        exts: &["jpg", "jpeg", "jpe", "jfif"],
        windows_has_none: false,
    },
    FormatGroup {
        id: "gif",
        label: "GIF",
        exts: &["gif"],
        windows_has_none: false,
    },
    FormatGroup {
        id: "bmp",
        label: "Bitmap",
        exts: &["bmp", "dib"],
        windows_has_none: false,
    },
    FormatGroup {
        id: "tiff",
        label: "TIFF",
        exts: &["tif", "tiff", "tx"],
        windows_has_none: false,
    },
    FormatGroup {
        id: "webp",
        label: "WebP",
        exts: &["webp"],
        windows_has_none: false,
    },
    FormatGroup {
        id: "ico",
        label: "Icon and cursor",
        exts: &["ico", "cur"],
        windows_has_none: false,
    },
    FormatGroup {
        id: "jxl",
        label: "JPEG XL",
        exts: &["jxl"],
        windows_has_none: false,
    },
    FormatGroup {
        id: "heif",
        label: "HEIF / HEIC",
        exts: &["heic", "heif"],
        windows_has_none: false,
    },
    FormatGroup {
        id: "avif",
        label: "AVIF",
        exts: &["avif"],
        windows_has_none: false,
    },
    FormatGroup {
        id: "raw",
        label: "Camera raw",
        exts: &[
            "cr2", "cr3", "crw", "nef", "nrw", "arw", "srf", "sr2", "raf", "orf", "rw2", "pef",
            "srw", "dng", "x3f", "3fr", "fff", "iiq", "erf", "mrw", "dcr", "kdc", "mef", "mos",
            "rwl", "gpr", "raw",
        ],
        windows_has_none: false,
    },
];

/// The group with this id.
pub fn group(id: &str) -> Option<&'static FormatGroup> {
    GROUPS.iter().find(|g| g.id == id)
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
        let decoder: BTreeSet<&str> = fire_decode::all_extensions().iter().copied().collect();
        let missing: Vec<_> = decoder.difference(&seen).collect();
        let extra: Vec<_> = seen.difference(&decoder).collect();
        assert!(
            missing.is_empty() && extra.is_empty(),
            "formats table and fire_decode::all_extensions() disagree.\n  no checkbox: {missing:?}\n  \
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

    /// The groups Windows has no thumbnail for come first, so both lists can split into "Windows
    /// has none" and "Windows has its own" under one heading each.
    #[test]
    fn windows_has_none_groups_lead() {
        let first_own = GROUPS.iter().position(|g| !g.windows_has_none).unwrap();
        assert!(GROUPS[first_own..].iter().all(|g| !g.windows_has_none));
    }

    #[test]
    fn parse_ids_keeps_order_drops_duplicates_and_reports_typos() {
        let (known, unknown) = parse_ids("tga, PSD;exr tga  pngg");
        assert_eq!(known, ["tga", "psd", "exr"]);
        assert_eq!(unknown, ["pngg"]);
        assert_eq!(parse_ids(""), (vec![], vec![]));
    }

    /// The installer's thumbnails page lists every group, in this order and in the column
    /// `windows_has_none` puts it in, under a caption that starts with its label. Read the
    /// `[Code]` list back and compare outright, the way `fire-decode` polices its association
    /// rows — an Inno script cannot `use` this table. Lines of the form
    /// `AddThumbFormat(True, 'tga', 'Truevision TGA (.tga)');`.
    #[test]
    fn installer_thumbnail_page_matches_the_table() {
        const ISS: &str = include_str!("../../../installer/fire.iss");
        let listed: Vec<(bool, String, String)> = ISS
            .lines()
            .filter_map(|l| l.trim().strip_prefix("AddThumbFormat("))
            .map(|args| {
                // The Boolean, then two quoted strings: split on the quotes.
                let parts: Vec<&str> = args.split('\'').collect();
                assert_eq!(parts.len(), 5, "unexpected AddThumbFormat line: {args}");
                let windows_has_none = parts[0].trim().trim_end_matches(',').trim() == "True";
                (windows_has_none, parts[1].to_string(), parts[3].to_string())
            })
            .collect();
        let script: Vec<(bool, &str)> = listed
            .iter()
            .map(|(none, id, _)| (*none, id.as_str()))
            .collect();
        let table: Vec<(bool, &str)> = GROUPS
            .iter()
            .map(|g| (g.windows_has_none, g.id))
            .collect();
        assert_eq!(
            script, table,
            "installer/fire.iss's ListThumbFormats differs from formats::GROUPS (column, id \
             or order)"
        );
        for (g, (.., caption)) in GROUPS.iter().zip(&listed) {
            assert!(
                caption.starts_with(g.label),
                "the installer captions {} {caption:?}, not after its label {:?}",
                g.id,
                g.label
            );
        }
    }
}
