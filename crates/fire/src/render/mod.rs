//! Render-side view model: the pure pan/zoom/fit math ([`view`]), the GPU image renderer ([`gpu`])
//! that presents it via a D3D11 flip-model swapchain, and the Dear ImGui layer ([`imgui`]) that
//! draws the chrome into the same backbuffer.
//!
//! These two are the only modules permitted to use the typed `windows` crate (COM); everything else
//! in the app uses raw `windows-sys`.

pub mod gpu;
pub mod imgui;
pub mod view;
