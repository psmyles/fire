//! Render-side view model: the pure pan/zoom/fit math ([`view`]), the GPU image renderer ([`gpu`])
//! that draws it through sokol_gfx, the device and swapchain sokol_gfx draws through ([`d3d11`] on
//! Windows, [`metal`] on macOS), the CPU mip chain sokol_gfx asks the app to supply ([`mips`]),
//! and the Dear ImGui layer ([`imgui`]) that draws the chrome into the same frame.
//!
//! These are the only modules that name `sokol::gfx` (and, in the platform glue, the native GPU
//! API); everything above them (`crate::ui`, the app) is GPU-API-free.
//!
//! [`backend`] is whichever of the two platform modules this build has. They are twins, not a
//! trait: the set of targets is closed and known, so an alias costs nothing at runtime and keeps
//! [`gpu`] free of `cfg`. Anything added to one must be added to the other — the contract is
//! `Device` (`create`, `fill_environment`), `Swapchain` (`new`, `size`, `resize`, `acquire`,
//! `present`) and `SWAPCHAIN_FORMAT`.

#[cfg(windows)]
pub mod d3d11;
/// The sokol_gfx shader reflection generated from `shader.glsl` — bindings, uniform-block layout
/// and per-backend entry points. Machine-written by `scripts/gen-shaders.sh`; never edit it.
#[path = "generated/shader.rs"]
pub mod generated_shader;
pub mod gpu;
pub mod imgui;
#[cfg(target_os = "macos")]
pub mod metal;
pub mod mips;
pub mod view;

#[cfg(windows)]
pub use d3d11 as backend;
#[cfg(target_os = "macos")]
pub use metal as backend;
