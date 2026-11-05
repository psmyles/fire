//! Two things from the repository root, the one place each is edited:
//! - the product version, from `product.json`, as `FIRE_VERSION`: the same value the viewer beside
//!   it reports. `--version` and every results file name it, and Asset Check's `requires`
//!   compares it against the version it needs;
//! - the rules, from `rules-catalog.toml`: asset-rules checks the catalog (no rule may be newer
//!   than this version) and generates `$OUT_DIR/rules.rs`, which `main.rs` includes as `rules`.

use std::env;
use std::path::{Path, PathBuf};

fn main() {
    let manifest = env::var("CARGO_MANIFEST_DIR").unwrap();
    let root = Path::new(&manifest).join("../..");

    let path = root.join("product.json");
    println!("cargo:rerun-if-changed={}", path.display());
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    let json: serde_json::Value = serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("{} is not valid JSON: {e}", path.display()));
    let version = json
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("product.json is missing the string field \"version\""));
    println!("cargo:rustc-env=FIRE_VERSION={version}");

    let catalog = root.join("rules-catalog.toml");
    println!("cargo:rerun-if-changed={}", catalog.display());
    let code =
        asset_rules::codegen::generate(&catalog, version).unwrap_or_else(|why| panic!("{why}"));
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("rules.rs");
    std::fs::write(&out, code).unwrap_or_else(|e| panic!("failed to write {}: {e}", out.display()));
}
