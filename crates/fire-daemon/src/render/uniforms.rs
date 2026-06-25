//! The `#[repr(C)]` uniform the image shader reads, and the CPU-side builder that turns
//! [`ViewState`] + [`DisplayState`] into it.
//!
//! Layout is all `vec4` (16-byte) members so the std140 uniform rules and the Rust struct
//! agree with zero padding guesswork — a frequent source of silent GPU corruption. The
//! vertex stage uses `transform`; the fragment stage uses the rest.

use fire_decode::PixelFormat;

use crate::render::view::{DisplayState, ViewState, Viewport};

// Fragment `flags` bits (mirrored in image.wgsl).
/// Source is float/linear HDR → exposure + tonemap apply.
pub const FLAG_HDR: u32 = 1 << 0;
/// Sampled RGB must be sRGB→linear decoded in-shader (the 16-bit-unorm path, which has no
/// hardware `_Srgb` texture variant; the 8-bit path uses an `Rgba8UnormSrgb` texture and
/// is decoded by the sampler).
pub const FLAG_SRGB_DECODE: u32 = 1 << 1;
/// Encode linear→sRGB in-shader (only when the surface is *not* an sRGB format and the
/// hardware won't do it on present). Normally off.
pub const FLAG_SRGB_ENCODE: u32 = 1 << 2;
/// Composite RGB over a checkerboard where the image is transparent.
pub const FLAG_CHECKER: u32 = 1 << 3;

/// Checkerboard cell size in surface pixels.
const CHECKER_SIZE: f32 = 12.0;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct ViewUniform {
    /// Maps the unit quad to the image's on-screen NDC rect: `ndc = pos.xy * .xy + .zw`.
    transform: [f32; 4],
    /// `w, h, 1/w, 1/h` of the image in pixels.
    image_size: [f32; 4],
    /// `w, h, _, _` of the full surface in pixels.
    viewport_size: [f32; 4],
    /// Linear background behind the image when the checker is off (rgb, a).
    bg_color: [f32; 4],
    /// `exposure(stops), checker_size(px), _, _`.
    params: [f32; 4],
    /// `tonemap, channel, flags, _`.
    modes: [u32; 4],
}

impl ViewUniform {
    /// Build the uniform for one frame. `surface_is_srgb` selects hardware vs in-shader
    /// final encode; `format` drives the HDR / sRGB-decode flags.
    pub fn build(
        view: &ViewState,
        display: &DisplayState,
        image: (u32, u32),
        vp: &Viewport,
        format: PixelFormat,
        surface_is_srgb: bool,
    ) -> Self {
        let (iw, ih) = (image.0.max(1) as f32, image.1.max(1) as f32);
        let (sw, sh) = (vp.width.max(1.0), vp.height.max(1.0));
        let c = vp.center();

        // On-screen image rect (surface px, y-down, origin top-left).
        let wpx = iw * view.zoom;
        let hpx = ih * view.zoom;
        let left = c.0 + view.pan.0 - wpx * 0.5;
        let top = c.1 + view.pan.1 - hpx * 0.5;

        // Pixel rect → NDC. NDC y is up, so the y scale is negated and the offset is
        // measured from the top.
        let transform = [
            wpx / sw * 2.0,
            -(hpx / sh * 2.0),
            left / sw * 2.0 - 1.0,
            1.0 - top / sh * 2.0,
        ];

        let mut flags = FLAG_CHECKER;
        if format.is_hdr() {
            flags |= FLAG_HDR;
        }
        if matches!(format, PixelFormat::Rgba16Unorm) {
            flags |= FLAG_SRGB_DECODE;
        }
        if !surface_is_srgb {
            flags |= FLAG_SRGB_ENCODE;
        }

        Self {
            transform,
            image_size: [iw, ih, 1.0 / iw, 1.0 / ih],
            viewport_size: [sw, sh, 0.0, 0.0],
            bg_color: [0.0, 0.0, 0.0, 1.0],
            params: [display.exposure, CHECKER_SIZE, 0.0, 0.0],
            modes: [display.tonemap.as_u32(), display.channel.as_u32(), flags, 0],
        }
    }
}
