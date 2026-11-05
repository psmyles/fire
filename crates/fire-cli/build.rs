//! Hands the product version to `fire-cli` from `product.json` (repo root), the one place it's
//! edited, as `FIRE_VERSION`: the same value the viewer beside it reports. `--version` and every
//! results file name it, and Asset Check's `requires` compares it against the version it needs.

use std::env;
use std::path::Path;

fn main() {
    let manifest = env::var("CARGO_MANIFEST_DIR").unwrap();
    let path = Path::new(&manifest).join("../../product.json");
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
}
