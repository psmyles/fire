//! Render-side view model: the pure pan/zoom/fit math ([`view`]) and the GPU image renderer
//! ([`gpu`]) that presents it via a D3D11 flip-model swapchain.

pub mod gpu;
pub mod view;
