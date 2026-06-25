//! Embeds the Fire app icon and version/product metadata into `fire.exe`'s Windows
//! resources, so Explorer shows the flame for the file and Task Manager / file
//! properties read "Fire". This covers the on-disk exe (which also supplies the
//! window/taskbar icon at runtime).

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
    // ProductName is the product family; FileDescription is the friendly name Task
    // Manager shows — both "Fire" now that it's a single self-contained app.
    res.set("ProductName", "Fire");
    res.set("FileDescription", "Fire");
    res.set("OriginalFilename", "fire.exe");
    res.compile().expect("failed to embed Windows resources");
}
