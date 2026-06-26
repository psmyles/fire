//! Build steps for `fire.exe`:
//!   1. Precompile the viewport HLSL to DXBC with `fxc` (the Windows SDK offline shader
//!      compiler), so the bytecode is embedded at build time instead of compiled at startup
//!      via `D3DCompile`. This drops the runtime `d3dcompiler` dependency, shaves the cold-start
//!      path, and turns a broken shader into a build error rather than a launch-time panic.
//!   2. Embed the Fire app icon and version/product metadata into the exe's Windows resources,
//!      so Explorer shows the flame and Task Manager / file properties read "Fire".

use std::path::{Path, PathBuf};

fn main() {
    // The crate is Windows-only; both steps need a Windows target and the Windows SDK.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    compile_shaders();
    embed_resources();
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

/// Embed the Fire `.ico` + product metadata into the exe (Explorer file icon, Task Manager name).
fn embed_resources() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let ico = Path::new(&manifest).join("../../assets/fire.ico");
    println!("cargo:rerun-if-changed={}", ico.display());

    let mut res = winresource::WindowsResource::new();
    res.set_icon(ico.to_str().expect("icon path is valid UTF-8"));
    // ProductName is the product family; FileDescription is the friendly name Task Manager
    // shows — both "Fire" now that it's a single self-contained app.
    res.set("ProductName", "Fire");
    res.set("FileDescription", "Fire");
    res.set("OriginalFilename", "fire.exe");
    res.compile().expect("failed to embed Windows resources");
}
