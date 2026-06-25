//! Embeds the Fire app icon and version/product metadata into the stub exe's Windows
//! resources. The stub is the Explorer target — the user-facing "Fire" viewer — so its
//! on-disk icon is what users see associated with image files once Phase 6 registers the
//! handler, and its friendly name is simply "Fire".

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let ico = std::path::Path::new(&manifest).join("../../assets/fire.ico");
    println!("cargo:rerun-if-changed={}", ico.display());

    let mut res = winresource::WindowsResource::new();
    res.set_icon(ico.to_str().expect("icon path is valid UTF-8"));
    // The launcher is the user-facing viewer: its friendly name is just "Fire".
    res.set("ProductName", "Fire");
    res.set("FileDescription", "Fire");
    res.set("OriginalFilename", "fire-stub.exe");
    res.compile().expect("failed to embed Windows resources");
}
