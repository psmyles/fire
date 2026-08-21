//! Product identity strings, sourced from `product.json` at build time.
//!
//! `build.rs` reads the canonical `product.json` and re-exports its fields as `FIRE_*`
//! compile-time env vars; these constants surface them to the rest of the app so every
//! user-facing string (window title, future About dialog) comes from that one file. Editing
//! `product.json` and rebuilding updates them everywhere — no string is hardcoded here.

/// Product/display name (e.g. shown in the window title and taskbar). From `productName`.
pub const NAME: &str = env!("FIRE_PRODUCT_NAME");

/// Product version, e.g. "0.3.0" (shown on the empty-window card). From `version`.
pub const VERSION: &str = env!("FIRE_VERSION");

/// One-line description ("Fast Image REview - …"); the empty-window card shows the long name
/// before the " - ". From `description`.
pub const DESCRIPTION: &str = env!("FIRE_DESCRIPTION");

// The other product.json fields (publisher, copyright, homepage) are exported by build.rs as
// FIRE_* env vars too; surface one here with `env!` the moment something user-facing (an About
// box, say) actually reads it. Constants for all of them sat here for a while annotated
// "surfaced by a future About/settings dialog" — the settings dialog then shipped without
// touching any, which is the fate of most speculative plumbing.
