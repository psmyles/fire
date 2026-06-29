//! Build steps for `fire.exe`:
//!   1. Precompile the viewport HLSL to DXBC with `fxc` (the Windows SDK offline shader
//!      compiler), so the bytecode is embedded at build time instead of compiled at startup
//!      via `D3DCompile`. This drops the runtime `d3dcompiler` dependency, shaves the cold-start
//!      path, and turns a broken shader into a build error rather than a launch-time panic.
//!   2. Read the canonical product metadata from `product.json` (repo root) and (a) embed it into
//!      the exe's Windows version resource + app icon — so Explorer shows the flame and Task
//!      Manager / file properties read the product name/version — and (b) re-export the same
//!      strings as `FIRE_*` compile-time env vars the app reads via `env!` (window title, etc.).
//!      `product.json` is the single source of truth: editing it there flows into the binary, and
//!      the installer build script reads the same file, so a version bump lives in exactly one place.

use std::path::{Path, PathBuf};

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
    // The crate is Windows-only; both steps need a Windows target and the Windows SDK.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let product = read_product();
    compile_shaders();
    rasterize_icons();
    embed_resources(&product);
    export_env(&product);
}

/// Master rasterization size (px) for each toolbar icon. The icon module embeds these square A8
/// coverage masks and downsamples them to the exact physical icon size per DPI at runtime, so a
/// single high-res master gives crisp icons at any scale. Keep in sync with `icons::MASTER`.
const ICON_MASTER: u32 = 64;

/// The toolbar SVG icons (in `../../assets/icons/`), by file stem. Each is rasterized to an
/// `<stem>.a8` file in `OUT_DIR` (a row-major `ICON_MASTER`²-byte coverage mask) that the icon
/// module embeds via `include_bytes!`. The list is the source of truth for the `icons::Icon` enum;
/// a missing SVG is a build error (the metadata is mandatory, like the shaders).
const ICON_STEMS: &[&str] = &[
    "icon_left", "icon_right", "icon_zoom_out", "icon_zoom_in", "icon_fit", "icon_1_1", "icon_RGB",
    "icon_rgba", "icon_R", "icon_G", "icon_B", "icon_A", "icon_aces", "icon_ev+", "icon_ev-",
    "icon_W", "icon_C", "icon_outline", "icon_open_with",
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

        let mut pixmap = resvg::tiny_skia::Pixmap::new(ICON_MASTER, ICON_MASTER)
            .expect("allocate icon pixmap");
        let size = tree.size();
        let transform = resvg::tiny_skia::Transform::from_scale(
            ICON_MASTER as f32 / size.width(),
            ICON_MASTER as f32 / size.height(),
        );
        resvg::render(&tree, transform, &mut pixmap.as_mut());

        // Keep only the alpha (coverage) byte of each RGBA texel — the mask the chrome tints.
        let alpha: Vec<u8> = pixmap.data().chunks_exact(4).map(|px| px[3]).collect();
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

/// Compile each entry point of `src/render/shader.hlsl` to a `.dxbc` in `OUT_DIR`, which
/// `render::gpu` embeds via `include_bytes!`. fxc targets shader model 5.x (`vs_5_0`/`ps_5_0`).
fn compile_shaders() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let hlsl = Path::new(&manifest).join("src/render/shader.hlsl");
    println!("cargo:rerun-if-changed={}", hlsl.display());
    println!("cargo:rerun-if-env-changed=FXC");

    let fxc = find_fxc();
    for (entry, target) in [("vs_main", "vs_5_0"), ("ps_main", "ps_5_0")] {
        let out = Path::new(&out_dir).join(format!("{entry}.dxbc"));
        let output = std::process::Command::new(&fxc)
            .args(["/nologo", "/O3", "/T", target, "/E", entry, "/Fo"])
            .arg(&out)
            .arg(&hlsl)
            .output()
            .unwrap_or_else(|e| panic!("failed to run fxc ({}): {e}", fxc.display()));
        if !output.status.success() {
            panic!(
                "fxc failed to compile {entry} ({target}):\n{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }
    }
}

/// Locate `fxc.exe`. Honors an explicit `FXC` override, then the SDK bin path exported inside a
/// Developer Command Prompt (`WindowsSdkVerBinPath`), then the newest installed Windows 10/11 SDK
/// bin, and finally falls back to `fxc.exe` on `PATH`.
fn find_fxc() -> PathBuf {
    if let Ok(p) = std::env::var("FXC") {
        return PathBuf::from(p);
    }
    if let Ok(bin) = std::env::var("WindowsSdkVerBinPath") {
        let p = PathBuf::from(bin).join("x64").join("fxc.exe");
        if p.exists() {
            return p;
        }
    }
    // Search every installed SDK version under each Program Files root; pick the newest.
    let mut candidates: Vec<(std::ffi::OsString, PathBuf)> = Vec::new();
    for var in ["ProgramFiles(x86)", "ProgramFiles"] {
        let Ok(pf) = std::env::var(var) else { continue };
        let bin = PathBuf::from(pf).join("Windows Kits").join("10").join("bin");
        let Ok(entries) = std::fs::read_dir(&bin) else { continue };
        for e in entries.flatten() {
            let p = e.path().join("x64").join("fxc.exe");
            if p.exists() {
                candidates.push((e.file_name(), p)); // key on the version dir name
            }
        }
    }
    candidates.sort_by(|a, b| a.0.cmp(&b.0));
    candidates
        .pop()
        .map(|(_, p)| p)
        .unwrap_or_else(|| PathBuf::from("fxc.exe"))
}

/// Embed the Fire `.ico` + product metadata into the exe (Explorer file icon, Task Manager name,
/// file-properties version tab). Every string comes from `product.json` so the binary's metadata
/// can never drift from the installer's.
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
/// (`major<<48 | minor<<32 | patch<<16 | build`). Missing components default to 0.
fn packed_version(version: &str) -> u64 {
    let mut parts = version.split('.').map(|s| s.parse::<u64>().unwrap_or(0));
    let mut next = || parts.next().unwrap_or(0);
    (next() << 48) | (next() << 32) | (next() << 16) | next()
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
