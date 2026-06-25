//! Render-side view model: the pure pan/zoom/fit math ([`view`]) and the GPU uniform it
//! feeds ([`uniforms`]). The wgpu surface/pipeline that consumes them lives in
//! [`crate::gpu`].

pub mod uniforms;
pub mod view;
