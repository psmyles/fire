//! Render-side view model: the pure pan/zoom/fit math ([`view`]), the GPU image renderer ([`gpu`])
//! that draws it through sokol_gfx, the device and swapchain sokol_gfx draws through ([`d3d11`] on
//! Windows), the CPU mip chain sokol_gfx asks the app to supply ([`mips`]), and the Dear ImGui
//! layer ([`imgui`]) that draws the chrome into the same frame.
//!
//! These are the only modules that name `sokol::gfx` (and, in the platform glue, the native GPU
//! API); everything above them (`crate::ui`, the app) is GPU-API-free.

#[cfg(windows)]
pub mod d3d11;
pub mod gpu;
pub mod imgui;
pub mod mips;
pub mod view;
