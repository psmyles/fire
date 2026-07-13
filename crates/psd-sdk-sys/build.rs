//! Build script for psd-sdk-sys.
//!
//! - bindgen generates Rust FFI declarations from the C-ABI `wrapper.h`. The header is
//!   C-style (`<stdint.h>` only) so clang-18's parse never touches the MSVC STL (avoids
//!   STL1000); `_ALLOW_COMPILER_AND_STL_VERSION_MISMATCH` is passed as belt-and-suspenders.
//! - cc compiles the vendored psd_sdk C++ plus `wrapper.cpp` with MSVC into a static lib.
//!   `cl.exe` (not clang) compiles the C++, so the STL constraint does not apply there.

use std::env;
use std::path::PathBuf;

fn main() {
    let crate_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let wrapper_h = crate_dir.join("wrapper.h");
    let wrapper_cpp = crate_dir.join("wrapper.cpp");
    let psd_dir = crate_dir.join("vendor").join("Psd");

    println!("cargo:rerun-if-changed={}", wrapper_h.display());
    println!("cargo:rerun-if-changed={}", wrapper_cpp.display());
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=vendor");

    // Windows-only, like the rest of the workspace: psd_sdk and `wrapper.cpp` are compiled by MSVC
    // (the vendored sources exclude the POSIX/Obj-C++ platform files). Short-circuit on any other
    // host rather than failing deep inside bindgen or cl.exe with something unrecognisable.
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    // --- bindgen: C-ABI wrapper header -> Rust FFI declarations ------------------
    let bindings = bindgen::Builder::default()
        .header(wrapper_h.to_string_lossy())
        .clang_arg("-x")
        .clang_arg("c++")
        .clang_arg("-std=c++17")
        .clang_arg("-D_ALLOW_COMPILER_AND_STL_VERSION_MISMATCH")
        .allowlist_function("fire_psd_.*")
        .allowlist_type("fire_psd.*")
        .generate()
        .expect("bindgen failed — check that libclang.dll is on PATH / LIBCLANG_PATH is set");
    bindings
        .write_to_file(out_dir.join("bindings.rs"))
        .expect("failed to write generated bindings.rs");

    // --- cc: compile vendored psd_sdk + wrapper.cpp -----------------------------
    if !psd_dir.join("Psd.h").exists() {
        panic!(
            "psd_sdk source missing at {} — vendor it (see vendor/VENDOR.txt)",
            psd_dir.display()
        );
    }

    let mut build = cc::Build::new();
    build
        .cpp(true)
        .include(&psd_dir)
        .file(&wrapper_cpp)
        .warnings(false); // third-party code; don't fail/noise on its warnings

    // MSVC C++17 + exceptions. /MD (the cc default) matches Rust's default CRT.
    build.flag_if_supported("/std:c++17");
    build.flag_if_supported("/EHsc");
    // For non-MSVC toolchains (not our target, but keeps the script honest):
    build.flag_if_supported("-std=c++17");

    for entry in std::fs::read_dir(&psd_dir).expect("read vendor/Psd") {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("cpp") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        // Exclude non-Windows platform sources (POSIX aio / Objective-C++).
        if name.ends_with("_Linux.cpp") || name.ends_with("_Mac.cpp") {
            continue;
        }
        build.file(&path);
    }

    build.compile("fire_psd");
}
