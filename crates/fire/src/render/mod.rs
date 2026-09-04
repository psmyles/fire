//! Render-side view model: the pure pan/zoom/fit math ([`view`]), the GPU image renderer ([`gpu`])
//! that presents it through a wgpu surface, the mip-chain blit wgpu lacks ([`mipgen`]), and the
//! Dear ImGui layer ([`imgui`]) that draws the chrome into the same frame.
//!
//! These are the only modules that name `wgpu`; everything above them (`crate::ui`, the app) is
//! GPU-API-free.

pub mod gpu;
pub mod imgui;
pub mod mipgen;
pub mod view;
