//! Product identity strings, sourced from `product.json` at build time.
//!
//! `build.rs` reads the canonical `product.json` and re-exports its fields as `FIRE_*`
//! compile-time env vars; these constants surface them to the rest of the app so every
//! user-facing string (window title, future About dialog) comes from that one file. Editing
//! `product.json` and rebuilding updates them everywhere — no string is hardcoded here.

/// Product/display name (e.g. shown in the window title and taskbar). From `productName`.
pub const NAME: &str = env!("FIRE_PRODUCT_NAME");

/// Marketing version string (e.g. "0.1.0"). From `version`.
#[allow(dead_code)] // surfaced by a future About/settings dialog
pub const VERSION: &str = env!("FIRE_VERSION");

/// One-line product description. From `description`.
#[allow(dead_code)] // surfaced by a future About/settings dialog
pub const DESCRIPTION: &str = env!("FIRE_DESCRIPTION");

/// Publisher / company name. From `publisher`.
#[allow(dead_code)] // surfaced by a future About/settings dialog
pub const PUBLISHER: &str = env!("FIRE_PUBLISHER");

/// Copyright line. From `copyright`.
#[allow(dead_code)] // surfaced by a future About/settings dialog
pub const COPYRIGHT: &str = env!("FIRE_COPYRIGHT");

/// Project homepage URL. From `homepage`.
#[allow(dead_code)] // surfaced by a future About/settings dialog
pub const HOMEPAGE: &str = env!("FIRE_HOMEPAGE");
