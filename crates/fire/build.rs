//! Build steps for the `fire` executable:
//!   1. Precompile the viewport shader to bytecode for *this host's* backend, from the per-backend
//!      sources `scripts/gen-shaders.sh` generated into `src/render/generated/` out of the one
//!      annotated-GLSL source `src/render/shader.glsl`: HLSL → DXBC with `fxc` (the Windows SDK
//!      compiler) on Windows, MSL → one `.metallib` per stage with `xcrun metal` + `metallib` on
//!      macOS. So the bytecode is embedded at build time instead of compiled at startup: nothing
//!      on the cold-start path on either OS, and a broken shader is a build error rather than a
//!      launch-time failure.
//!   2. Compile `simgui/simgui.c` — sokol_imgui.h, Dear ImGui's sokol backend — as C, against the
//!      vendored sokol headers and the cimgui header `dear-imgui-sys` links. The Rust side calls it
//!      through a handful of `extern "C"` declarations in `render::imgui`.
//!   3. Rasterize the toolbar icons and decode the logo, and read the canonical product metadata
//!      from `product.json` (repo root): (a) on Windows embed it into the exe's version resource +
//!      app icon, and (b) re-export the same strings as `FIRE_*` compile-time env vars the app
//!      reads via `env!`. `product.json` is the single source of truth; the packaging scripts read
//!      it too.

use std::env;
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

#[cfg(not(windows))]
fn embed_resources(_p: &Product) {}

fn main() {
    let product = read_product();
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "windows" {
        compile_shaders();
    }
    #[cfg(target_os = "macos")]
    if target_os == "macos" {
        compile_shaders_metal();
    }
    compile_simgui(&target_os);
    rasterize_icons();
    decode_logo();
    if target_os == "windows" {
        embed_resources(&product);
    }
    export_env(&product);
}

/// Compile sokol_imgui.h (see `simgui/simgui.c`). The sokol backend define must match the one
/// the vendored `sokol` crate's build picks — the same `SOKOL_BACKEND` override, the same per-OS
/// default — because the two translation units share sokol_gfx's structs.
fn compile_simgui(target_os: &str) {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let simgui = manifest.join("simgui");
    let sokol_c = manifest.join("../../vendor/sokol-rust/src/sokol/c");
    for f in ["simgui.c", "sokol_imgui.h", "cimgui.h"] {
        println!("cargo:rerun-if-changed={}", simgui.join(f).display());
    }
    println!("cargo:rerun-if-env-changed=SOKOL_BACKEND");

    let backend = match env::var("SOKOL_BACKEND").as_deref() {
        Ok("D3D11") => "SOKOL_D3D11",
        Ok("METAL") => "SOKOL_METAL",
        Ok("GL") => "SOKOL_GLCORE",
        Ok("GLES3") => "SOKOL_GLES3",
        Ok("WGPU") => "SOKOL_WGPU",
        _ => match target_os {
            "windows" => "SOKOL_D3D11",
            "macos" => "SOKOL_METAL",
            _ => "SOKOL_GLCORE",
        },
    };

    let mut build = cc::Build::new();
    build
        .file(simgui.join("simgui.c"))
        .include(&simgui)
        .include(&sokol_c)
        .define(backend, None)
        // Renderer only: the window and its input are winit's (see render::imgui).
        .define("SOKOL_IMGUI_NO_SOKOL_APP", None)
        // Must match dear-imgui-sys's own compile of cimgui/imgui, or the struct layouts differ.
        .define("CIMGUI_DEFINE_ENUMS_AND_STRUCTS", None)
        .define("CIMGUI_NO_EXPORT", None)
        .define("IMGUI_USE_WCHAR32", None)
        .define("IMGUI_DISABLE_OBSOLETE_FUNCTIONS", None)
        .warnings(false);
    if env::var("PROFILE").as_deref() == Ok("release") {
        build.define("NDEBUG", None);
        build.opt_level(2);
    }
    build.compile("fire_simgui");
}

/// Edge (px) of the empty-window logo raster. Keep in sync with `render::imgui::LOGO_EDGE`.
const LOGO_EDGE: u32 = 256;

/// Decode the logo PNG (`assets/icon-256.png`, pre-sized from the 1024 px master) to raw
/// straight-alpha RGBA in `OUT_DIR`, which `render::imgui` embeds for the empty-window card and
/// the shell uses as the window icon. Decoded here so no PNG decoder ships in the exe and a
/// malformed asset is a build error, not a launch panic — the same posture as the icons and the
/// shaders.
fn decode_logo() {
    let manifest = env::var("CARGO_MANIFEST_DIR").unwrap();
    let out_dir = env::var("OUT_DIR").unwrap();
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
    let manifest = env::var("CARGO_MANIFEST_DIR").unwrap();
    let out_dir = env::var("OUT_DIR").unwrap();
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
    let manifest = env::var("CARGO_MANIFEST_DIR").unwrap();
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

/// Compile each entry point of the generated HLSL to a `.dxbc` in `OUT_DIR`, which
/// `render::gpu` embeds via `include_bytes!` and hands to sokol_gfx as shader bytecode. fxc
/// targets shader model 5.0 (`vs_5_0`/`ps_5_0`), which is what sokol's D3D11 backend expects.
fn compile_shaders() {
    let manifest = env::var("CARGO_MANIFEST_DIR").unwrap();
    let out_dir = env::var("OUT_DIR").unwrap();
    let gen = Path::new(&manifest).join("src/render/generated");
    // The one source is `shader.glsl`, but cargo never compiles it: `scripts/gen-shaders.sh`
    // turns it into the per-backend sources below, which are checked in (D4). Rebuild when
    // either changes, so an edit to the .glsl that was not regenerated is still noticed here.
    println!("cargo:rerun-if-changed={}", gen.display());
    println!(
        "cargo:rerun-if-changed={}",
        Path::new(&manifest)
            .join("src/render/shader.glsl")
            .display()
    );
    println!("cargo:rerun-if-env-changed=FXC");

    let fxc = find_fxc();
    // SPIRV-Cross emits one file per stage, each with entry point `main`.
    for (stage, target) in [("vertex", "vs_5_0"), ("fragment", "ps_5_0")] {
        let src = gen.join(format!("shader_viewport_hlsl5_{stage}.hlsl"));
        let out = Path::new(&out_dir).join(format!("{stage}.dxbc"));
        let output = std::process::Command::new(&fxc)
            .args(["/nologo", "/O3", "/T", target, "/E", "main", "/Fo"])
            .arg(&out)
            .arg(&src)
            .output()
            .unwrap_or_else(|e| panic!("failed to run fxc ({}): {e}", fxc.display()));
        if !output.status.success() {
            panic!(
                "fxc failed to compile {} ({target}):\n{}{}",
                src.display(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }
    }
}

/// Compile the generated MSL to a `.metallib` per stage with `xcrun metal` + `metallib` (D24), so
/// nothing compiles a shader on the cold-start path. One library per stage rather than one for
/// both: SPIRV-Cross names every entry point `main0`, so two functions cannot share a library.
///
/// The Metal toolchain is a **separate download** on Xcode 16+ (`xcodebuild -downloadComponent
/// MetalToolchain`); `xcrun --find metal` succeeds without it, because what it finds is a stub
/// that fails only when run. That is why the failure below quotes the tool's own stderr.
#[cfg(target_os = "macos")]
fn compile_shaders_metal() {
    let manifest = env::var("CARGO_MANIFEST_DIR").unwrap();
    let out_dir = env::var("OUT_DIR").unwrap();
    let gen = Path::new(&manifest).join("src/render/generated");
    println!("cargo:rerun-if-changed={}", gen.display());
    println!(
        "cargo:rerun-if-changed={}",
        Path::new(&manifest)
            .join("src/render/shader.glsl")
            .display()
    );

    for stage in ["vertex", "fragment"] {
        let src = gen.join(format!("shader_viewport_metal_macos_{stage}.metal"));
        let air = Path::new(&out_dir).join(format!("{stage}.air"));
        let lib = Path::new(&out_dir).join(format!("{stage}.metallib"));

        run_metal_tool("metal", &["-c", "-O2"], &src, &air);
        run_metal_tool("metallib", &[], &air, &lib);
    }
}

/// One `xcrun -sdk macosx <tool> <args> -o <out> <input>` step of the Metal compile.
#[cfg(target_os = "macos")]
fn run_metal_tool(tool: &str, args: &[&str], input: &Path, out: &Path) {
    let output = std::process::Command::new("xcrun")
        .args(["-sdk", "macosx", tool])
        .args(args)
        .arg("-o")
        .arg(out)
        .arg(input)
        .output()
        .unwrap_or_else(|e| panic!("failed to run `xcrun {tool}`: {e}"));
    if !output.status.success() {
        panic!(
            "`xcrun {tool}` failed on {}:\n{}{}\n\nIf this says the Metal Toolchain is \
             missing, install it with `xcodebuild -downloadComponent MetalToolchain`.",
            input.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}

/// Locate `fxc.exe`. Honors an explicit `FXC` override, then the SDK bin path exported inside a
/// Developer Command Prompt (`WindowsSdkVerBinPath`), then the newest installed Windows 10/11 SDK
/// bin, and finally falls back to `fxc.exe` on `PATH`.
fn find_fxc() -> PathBuf {
    if let Ok(p) = env::var("FXC") {
        return PathBuf::from(p);
    }
    if let Ok(bin) = env::var("WindowsSdkVerBinPath") {
        let p = PathBuf::from(bin).join("x64").join("fxc.exe");
        if p.exists() {
            return p;
        }
    }
    // Search every installed SDK version under each Program Files root; pick the newest.
    let mut candidates: Vec<(std::ffi::OsString, PathBuf)> = Vec::new();
    for var in ["ProgramFiles(x86)", "ProgramFiles"] {
        let Ok(pf) = env::var(var) else { continue };
        let bin = PathBuf::from(pf)
            .join("Windows Kits")
            .join("10")
            .join("bin");
        let Ok(entries) = std::fs::read_dir(&bin) else {
            continue;
        };
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
        .map_or_else(|| PathBuf::from("fxc.exe"), |(_, p)| p)
}

/// Embed the Fire `.ico` + product metadata into the exe (Explorer file icon, Task Manager name,
/// file-properties version tab). Every string comes from `product.json` so the binary's metadata
/// can never drift from the installer's.
///
/// Gated on `cfg(windows)` — the *host* — not on `target_os`: a build script is compiled for the
/// host, so `winresource` (a `cfg(windows)` build-dependency) is only in scope on a Windows host
/// and a runtime `if` around this call would still have to compile off it. The caller keeps its
/// `target_os` check, so the pair reads as "Windows target, built on Windows"; a cross-build from
/// another host would no-op rather than embed, which is the honest outcome — the tool isn't there.
#[cfg(windows)]
fn embed_resources(p: &Product) {
    let manifest = env::var("CARGO_MANIFEST_DIR").unwrap();
    let ico = Path::new(&manifest).join("../../assets/fire.ico");
    println!("cargo:rerun-if-changed={}", ico.display());

    let mut res = winresource::WindowsResource::new();
    // set_icon embeds the .ico with resource id "1" (winresource's DEFAULT_APPLICATION_ICON_ID),
    // which Explorer shows for the file; the window/taskbar icon is the logo raster, set at runtime.
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
///
/// `cfg(windows)` for the same reason as [`embed_resources`], its only caller.
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
