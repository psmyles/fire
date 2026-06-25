//! Render-side view model: the pure pan/zoom/fit math ([`view`]), the CPU image shader
//! ([`shade`]), and the softbuffer surface that presents it ([`surface`]).

pub mod shade;
pub mod surface;
pub mod view;
