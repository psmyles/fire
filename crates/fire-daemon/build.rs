//! Embeds the Fire app icon and version/product metadata into the daemon exe's
//! Windows resources (so Explorer shows the flame for the file, and Task Manager /
//! file properties read "Fire Daemon" — the resident process, distinct from the
//! user-facing "Fire" launcher). The runtime taskbar/title-bar icon is set separately
//! from a raw RGBA blob in `app.rs`; this covers the on-disk exe.

fn main() {
    // Only meaningful for a Windows target (rc.exe / the resource section).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let ico = std::path::Path::new(&manifest).join("../../assets/fire.ico");
    println!("cargo:rerun-if-changed={}", ico.display());

    let mut res = winresource::WindowsResource::new();
    res.set_icon(ico.to_str().expect("icon path is valid UTF-8"));
    // ProductName is the product family ("Fire"); FileDescription is the per-binary
    // friendly name Task Manager shows — the daemon is "Fire Daemon".
    res.set("ProductName", "Fire");
    res.set("FileDescription", "Fire Daemon");
    res.set("OriginalFilename", "fire-daemon.exe");
    res.compile().expect("failed to embed Windows resources");
}
