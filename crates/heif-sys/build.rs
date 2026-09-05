//! Build script for heif-sys.
//!
//! - bindgen generates Rust FFI declarations from the C-ABI `wrapper.h`. The header is
//!   C-only (`<stdint.h>`/`<stddef.h>`), parsed as C, so clang never touches a C++ STL
//!   header (the STL1000 clang-version issue that psd-sdk-sys works around can't arise).
//! - cc compiles `wrapper.c` with the vendored libheif headers on the include path, into a
//!   static lib.
//! - We link the prebuilt vendored static libs (libheif + libde265 + dav1d).
//!
//! The vendored artifacts are per-target: one directory per vendored target, each holding the
//! headers *and* the libs produced by the same vcpkg run, because the two targets are not
//! necessarily at the same upstream version and bindgen must parse the headers that match the
//! libs it will link. See `vendor/VENDOR.txt` for the recipe.

use std::env;
use std::path::PathBuf;

/// A vendored target: which `vendor/` subdirectory holds it, and how it links.
struct Vendor {
    /// Subdirectory of `vendor/`, holding `include/` and `lib/`.
    dir: &'static str,
    /// Static libs to link, by cargo link name (so `heif` is `heif.lib` / `libheif.a`).
    libs: &'static [&'static str],
    /// The C++ runtime to name explicitly, where the object format cannot request it itself.
    cpp_runtime: Option<&'static str>,
}

fn vendor_for(target_os: &str, target_arch: &str) -> Option<Vendor> {
    match (target_os, target_arch) {
        // MSVC objects embed `/DEFAULTLIB` directives for the C++ runtime that libheif and
        // libde265 need, so it links itself and must not be named here.
        ("windows", "x86_64") => Some(Vendor {
            dir: "x64-windows",
            libs: &["heif", "libde265", "dav1d"],
            cpp_runtime: None,
        }),
        // Mach-O has no equivalent of `/DEFAULTLIB`, so the C++ runtime is named explicitly or
        // libheif's and libde265's C++ symbols go unresolved at link time.
        ("macos", "aarch64") => Some(Vendor {
            dir: "arm64-macos",
            libs: &["heif", "de265", "dav1d"],
            cpp_runtime: Some("c++"),
        }),
        _ => None,
    }
}

fn main() {
    let crate_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let wrapper_h = crate_dir.join("wrapper.h");
    let wrapper_c = crate_dir.join("wrapper.c");
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();

    println!("cargo:rerun-if-changed={}", wrapper_h.display());
    println!("cargo:rerun-if-changed={}", wrapper_c.display());
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=vendor");

    // Only the vendored targets can build. Fail here, naming the target, rather than deep inside
    // bindgen or the linker with something unrecognisable.
    let Some(vendor_target) = vendor_for(&target_os, &target_arch) else {
        std::fs::write(
            out_dir.join("bindings.rs"),
            format!(
                "compile_error!(\"heif-sys has no vendored libheif stack for {target_arch}-\
                 {target_os}: see crates/heif-sys/vendor/VENDOR.txt for the recipe\");\n"
            ),
        )
        .expect("failed to write stub bindings.rs");
        return;
    };

    let vendor = crate_dir.join("vendor").join(vendor_target.dir);
    let inc = vendor.join("include");
    let lib = vendor.join("lib");

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
        .expect("bindgen failed — check that libclang is on PATH / LIBCLANG_PATH is set");
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
    for name in vendor_target.libs {
        println!("cargo:rustc-link-lib=static={name}");
    }
    if let Some(runtime) = vendor_target.cpp_runtime {
        println!("cargo:rustc-link-lib=dylib={runtime}");
    }
}
