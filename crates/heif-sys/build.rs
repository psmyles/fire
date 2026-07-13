//! Build script for heif-sys.
//!
//! - bindgen generates Rust FFI declarations from the C-ABI `wrapper.h`. The header is
//!   C-only (`<stdint.h>`/`<stddef.h>`), parsed as C, so clang never touches a C++ STL
//!   header (the STL1000 clang-version issue that psd-sdk-sys works around can't arise).
//! - cc compiles `wrapper.c` with MSVC, with the vendored libheif headers on the include
//!   path, into a static lib.
//! - We link the prebuilt vendored static libs (libheif + libde265 + dav1d). The MSVC
//!   objects in those libs embed `/DEFAULTLIB` directives for the C++ runtime (libheif and
//!   libde265 are C++), so the C++ stdlib links automatically without naming it here.
//!
//! The vendored libs are produced once via vcpkg; see `vendor/VENDOR.txt` for the recipe.

use std::env;
use std::path::PathBuf;

fn main() {
    let crate_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let wrapper_h = crate_dir.join("wrapper.h");
    let wrapper_c = crate_dir.join("wrapper.c");
    let vendor = crate_dir.join("vendor");
    let inc = vendor.join("include");
    let lib = vendor.join("lib");

    println!("cargo:rerun-if-changed={}", wrapper_h.display());
    println!("cargo:rerun-if-changed={}", wrapper_c.display());
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=vendor");

    // Windows-only, like the rest of the workspace: the vendored libs are MSVC static libs and
    // `wrapper.c` is compiled by cl.exe. Short-circuit on any other host rather than failing deep
    // inside bindgen or the linker with something unrecognisable.
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    if !inc.join("libheif").join("heif.h").exists() {
        panic!(
            "libheif headers missing at {} — vendor the static build (see vendor/VENDOR.txt)",
            inc.display()
        );
    }

    // --- bindgen: C-ABI wrapper header -> Rust FFI declarations ------------------
    let bindings = bindgen::Builder::default()
        .header(wrapper_h.to_string_lossy())
        .clang_arg("-x")
        .clang_arg("c")
        .allowlist_function("fire_heif_.*")
        .allowlist_type("fire_heif_.*")
        .generate()
        .expect("bindgen failed — check that libclang.dll is on PATH / LIBCLANG_PATH is set");
    bindings
        .write_to_file(out_dir.join("bindings.rs"))
        .expect("failed to write generated bindings.rs");

    // --- cc: compile wrapper.c against the vendored libheif headers --------------
    cc::Build::new()
        .file(&wrapper_c)
        .include(&inc)
        .warnings(false)
        .compile("fire_heif");

    // --- link the vendored static decoder stack ---------------------------------
    println!("cargo:rustc-link-search=native={}", lib.display());
    // Order matters for some linkers (dependents before dependencies); MSVC resolves
    // across the whole set regardless, but keep heif -> codecs ordering for clarity.
    for name in ["heif", "libde265", "dav1d"] {
        println!("cargo:rustc-link-lib=static={name}");
    }
}
