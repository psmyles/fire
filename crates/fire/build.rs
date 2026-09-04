//! Build steps for the `fire` executable:
//!   1. Rasterize the toolbar SVGs and decode the logo PNG into raw pixels, so no SVG/PNG decoder
//!      ships in the exe and a malformed asset is a build error rather than a launch panic.
//!   2. Read the canonical product metadata from `product.json` (repo root) and (a) on Windows,
//!      embed it into the exe's version resource + app icon — so Explorer shows the flame and Task
//!      Manager / file properties read the product name/version — and (b) re-export the same
//!      strings as `FIRE_*` compile-time env vars the app reads via `env!` (window title, etc.).
//!      `product.json` is the single source of truth: editing it there flows into the binary, and
//!      the installer build scripts read the same file, so a version bump lives in exactly one place.
//!
//! There is no shader step any more: the viewport shader is WGSL (`src/render/shader.wgsl`),
//! which wgpu validates and compiles at pipeline creation on every OS.

use std::path::Path;

/// Product metadata read from `product.json`. Only the fields the build consumes are pulled.
struct Product {
    name: String,
    version: String,
    publisher: String,
    description: String,
    copyright: String,
    homepage: String,
}

fn main() {
    let product = read_product();
    rasterize_icons();
    decode_logo();
    #[cfg(windows)]
    embed_resources(&product);
    export_env(&product);
}

/// Edge (px) of the empty-window logo raster. Keep in sync with `render::imgui::LOGO_EDGE`.
const LOGO_EDGE: u32 = 256;

/// Decode the logo PNG (`assets/icon-256.png`, pre-sized from the 1024 px master) to raw
/// straight-alpha RGBA in `OUT_DIR`, which `render::imgui` embeds for the empty-window card.
/// Decoded here so no PNG decoder ships in the exe and a malformed asset is a build error,
/// not a launch panic — the same posture as the icons and the shaders.
fn decode_logo() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let png = Path::new(&manifest).join("../../assets/icon-256.png");
    println!("cargo:rerun-if-changed={}", png.display());

    let data = std::fs::read(&png)
        .unwrap_or_else(|e| panic!("failed to read logo {}: {e}", png.display()));
    let pixmap = resvg::tiny_skia::Pixmap::decode_png(&data)
        .unwrap_or_else(|e| panic!("{} is not valid PNG: {e}", png.display()));
    assert_eq!(
        (pixmap.width(), pixmap.height()),
        (LOGO_EDGE, LOGO_EDGE),
        "logo raster must be {LOGO_EDGE}×{LOGO_EDGE}"
    );

    // tiny-skia hands back premultiplied alpha; ImGui blends straight alpha, so demultiply.
    let mut rgba = Vec::with_capacity((LOGO_EDGE * LOGO_EDGE * 4) as usize);
    for px in pixmap.pixels() {
        let c = px.demultiply();
        rgba.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
    }
    let out = Path::new(&out_dir).join("logo.rgba");
    std::fs::write(&out, &rgba)
        .unwrap_or_else(|e| panic!("failed to write {}: {e}", out.display()));
}

/// Master rasterization size (px) for each toolbar icon. The icon module embeds these square A8
/// coverage masks and downsamples them to the exact physical icon size per DPI at runtime, so a
/// single high-res master gives crisp icons at any scale. Keep in sync with `icons::MASTER`.
const ICON_MASTER: u32 = 64;

/// The toolbar SVG icons (in `../../assets/icons/`), by file stem. Each is rasterized to an
/// `<stem>.a8` file in `OUT_DIR` (a row-major `ICON_MASTER`²-byte coverage mask) that the icon
/// module embeds via `include_bytes!`. The list is the source of truth for the `icons::Icon` enum;
/// a missing SVG is a build error (the metadata is mandatory, like the shaders).
///
/// This is a list of *positions*, not a set: a stem may appear twice, which is how two `Icon`
/// variants drawn from one SVG (the blue channel and the black backdrop) get a cell each and so a
/// `[icon_scale]` entry each. Rasterizing that stem twice writes the same bytes to the same file.
const ICON_STEMS: &[&str] = &[
    "icon_left",
    "icon_right",
    "icon_zoom_out",
    "icon_zoom_in",
    "icon_fit",
    "icon_1_1",
    "icon_RGB",
    "icon_rgba",
    "icon_R",
    "icon_G",
    "icon_B",
    "icon_A",
    "icon_aces",
    "icon_ev+",
    "icon_ev0",
    "icon_ev-",
    "icon_B",
    "icon_W",
    "icon_G",
    "icon_C",
    "icon_outline",
    "icon_open_with",
    "icon_fullscreen",
    "icon_flipbook",
    "icon_play",
    "icon_pause",
    "icon_more",
    "icon_octagon",
];

/// Rasterize each toolbar SVG to a square A8 coverage mask in `OUT_DIR`. The SVGs are single-color
/// (white) on transparent, so the rendered alpha channel *is* the coverage the chrome tints per
/// button state. Done at build time (resvg is a build-dep only) so no SVG rasterizer ships in the
/// exe and a malformed icon is a build error, not a launch panic — the same posture as the shaders.
fn rasterize_icons() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let icons = Path::new(&manifest).join("../../assets/icons");
    println!("cargo:rerun-if-changed={}", icons.display());

    let opt = resvg::usvg::Options::default();
    for stem in ICON_STEMS {
        let svg = icons.join(format!("{stem}.svg"));
        println!("cargo:rerun-if-changed={}", svg.display());
        let data = std::fs::read(&svg)
            .unwrap_or_else(|e| panic!("failed to read icon {}: {e}", svg.display()));
        let tree = resvg::usvg::Tree::from_data(&data, &opt)
            .unwrap_or_else(|e| panic!("{} is not valid SVG: {e}", svg.display()));

        let mut pixmap =
            resvg::tiny_skia::Pixmap::new(ICON_MASTER, ICON_MASTER).expect("allocate icon pixmap");
        let size = tree.size();
        let transform = resvg::tiny_skia::Transform::from_scale(
            ICON_MASTER as f32 / size.width(),
            ICON_MASTER as f32 / size.height(),
        );
        resvg::render(&tree, transform, &mut pixmap.as_mut());

        // Keep only the alpha (coverage) byte of each RGBA texel — the mask the chrome tints.
        let alpha: Vec<u8> = pixmap
            .data()
            .as_chunks::<4>()
            .0
            .iter()
            .map(|px| px[3])
            .collect();
        let out = Path::new(&out_dir).join(format!("{stem}.a8"));
        std::fs::write(&out, &alpha)
            .unwrap_or_else(|e| panic!("failed to write {}: {e}", out.display()));
    }
}

/// Parse `../../product.json` (repo root) into [`Product`]. Panics with a clear message on a
/// missing/malformed file or absent field — the metadata is mandatory, not best-effort.
fn read_product() -> Product {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let path = Path::new(&manifest).join("../../product.json");
    println!("cargo:rerun-if-changed={}", path.display());

    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    let json: serde_json::Value = serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("{} is not valid JSON: {e}", path.display()));

    let field = |key: &str| -> String {
        json.get(key)
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| panic!("product.json is missing the string field \"{key}\""))
            .to_string()
    };

    Product {
        name: field("productName"),
        version: field("version"),
        publisher: field("publisher"),
        description: field("description"),
        copyright: field("copyright"),
        homepage: field("homepage"),
    }
}

/// Embed the Fire `.ico` + product metadata into the exe (Explorer file icon, Task Manager name,
/// file-properties version tab). Every string comes from `product.json` so the binary's metadata
/// can never drift from the installer's. Windows only: the macOS bundle carries the same data in
/// its `Info.plist` (see `scripts/build-mac.sh`).
#[cfg(windows)]
fn embed_resources(p: &Product) {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let ico = Path::new(&manifest).join("../../assets/fire.ico");
    println!("cargo:rerun-if-changed={}", ico.display());

    let mut res = winresource::WindowsResource::new();
    // set_icon embeds the .ico with resource id "1" (winresource's DEFAULT_APPLICATION_ICON_ID),
    // which the app loads via LoadIconW(.., MAKEINTRESOURCE(1)) for the window/taskbar icon.
    res.set_icon(ico.to_str().expect("icon path is valid UTF-8"));
    // ProductName is the product family; FileDescription is the friendly name Task Manager shows.
    res.set("ProductName", &p.name);
    res.set("FileDescription", &p.name);
    res.set("CompanyName", &p.publisher);
    res.set("LegalCopyright", &p.copyright);
    res.set("Comments", &p.description);
    res.set("OriginalFilename", "fire.exe");
    res.set("InternalName", "fire");
    // Override winresource's Cargo-derived version strings + the numeric VS_FIXEDFILEINFO so the
    // file-properties version matches product.json regardless of the crate's Cargo version.
    res.set("FileVersion", &p.version);
    res.set("ProductVersion", &p.version);
    let packed = packed_version(&p.version);
    res.set_version_info(winresource::VersionInfo::FILEVERSION, packed);
    res.set_version_info(winresource::VersionInfo::PRODUCTVERSION, packed);
    res.compile().expect("failed to embed Windows resources");
}

/// Pack a dotted "major.minor.patch[.build]" string into the u64 VS_FIXEDFILEINFO layout
/// (`major<<48 | minor<<32 | patch<<16 | build`). Missing components default to 0; each field
/// is 16 bits, so a component above 65535 is clamped rather than silently bleeding into its
/// neighbour.
#[cfg(windows)]
fn packed_version(version: &str) -> u64 {
    let mut fields = [0u64; 4];
    for (field, part) in fields.iter_mut().zip(version.split('.')) {
        *field = part.parse::<u64>().unwrap_or(0).min(0xFFFF);
    }
    (fields[0] << 48) | (fields[1] << 32) | (fields[2] << 16) | fields[3]
}

/// Re-export the product strings as compile-time env vars (`FIRE_PRODUCT_NAME`, `FIRE_VERSION`, …)
/// so the app reads them via `env!` instead of hardcoding "Fire" or `CARGO_PKG_VERSION`. This keeps
/// every end-user-facing string (window title, future About dialog) sourced from product.json.
fn export_env(p: &Product) {
    println!("cargo:rustc-env=FIRE_PRODUCT_NAME={}", p.name);
    println!("cargo:rustc-env=FIRE_VERSION={}", p.version);
    println!("cargo:rustc-env=FIRE_PUBLISHER={}", p.publisher);
    println!("cargo:rustc-env=FIRE_DESCRIPTION={}", p.description);
    println!("cargo:rustc-env=FIRE_COPYRIGHT={}", p.copyright);
    println!("cargo:rustc-env=FIRE_HOMEPAGE={}", p.homepage);
}
