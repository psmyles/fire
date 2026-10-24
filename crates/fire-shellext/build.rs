//! Build script for fire-shellext: one linker flag.
//!
//! MSVC's linker warns (LNK4104) when the COM entry points `DllGetClassObject` / `DllCanUnloadNow`
//! are exported without `PRIVATE`, which only keeps them out of the *import library* — and nothing
//! links against this DLL; COM finds the exports by name at run time. rustc writes the `.def` file
//! itself and offers no way to mark an export private, so the warning is silenced instead of
//! left to train everyone to ignore linker output.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let windows = std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "windows");
    let msvc = std::env::var("CARGO_CFG_TARGET_ENV").is_ok_and(|env| env == "msvc");
    if windows && msvc {
        println!("cargo:rustc-cdylib-link-arg=/IGNORE:4104");
    }
}
