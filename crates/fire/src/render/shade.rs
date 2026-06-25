//! Pure-CPU image shader — the softbuffer render path's per-pixel pipeline, covering every
//! [`PixelFormat`] including HDR.
//!
//! Windowing-agnostic: it shades into a packed `0x00RRGGBB` framebuffer slice, so it is
//! unit-testable without a window. Per output pixel it inverse-maps into image space, fetches
//! a *linear* RGBA sample (nearest when magnifying, box-average over the minify footprint),
//! then runs the common tail, in order: HDR exposure `×2^stops` → tonemap (Reinhard/ACES) →
//! channel isolation → checkerboard composite over transparency → sRGB encode. softbuffer
//! presents raw bytes (no hardware sRGB), so the encode is always done here. The whole
//! pipeline runs in linear light.

use fire_decode::{DecodedImage, PixelFormat};

use crate::render::view::{Channel, DisplayState, Tonemap, ViewState, Viewport};

/// Checkerboard cell size in surface px.
const CHECKER_SIZE: f32 = 12.0;
/// Minify box-average footprint cap. Beyond this the source is undersampled (mild shimmer at
/// extreme zoom-out); kept small and bounded per the no-mip-chain decision.
const MAX_FOOT: i32 = 6;

/// Precomputed sRGB lookup tables, built once per `SurfaceState`.
pub struct Luts {
    /// sRGB byte (0..=255) → linear float.
    pub lin: [f32; 256],
    /// linear `[0,1]` → sRGB byte, indexed by `lin * 4096` (4097 entries).
    pub srgb: Vec<u8>,
}

impl Luts {
    pub fn new() -> Self {
        let mut lin = [0.0f32; 256];
        for (i, v) in lin.iter_mut().enumerate() {
            *v = srgb_to_linear(i as f32 / 255.0);
        }
        let mut srgb = vec![0u8; 4097];
        for (i, v) in srgb.iter_mut().enumerate() {
            *v = (linear_to_srgb(i as f32 / 4096.0).clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        }
        Self { lin, srgb }
    }
}

impl Default for Luts {
    fn default() -> Self {
        Self::new()
    }
}

/// Fan the framebuffer out across cores by horizontal bands and shade each. Cost is
/// O(surface pixels) — independent of source resolution except for the minify footprint.
#[allow(clippy::too_many_arguments)]
pub fn shade(
    buf: &mut [u32],
    w: u32,
    h: u32,
    img: &DecodedImage,
    view: &ViewState,
    display: &DisplayState,
    vp: &Viewport,
    luts: &Luts,
    bg: u32,
) {
    let total = (w as usize) * (h as usize);
    if buf.len() < total || w == 0 || h == 0 {
        return;
    }
    let buf = &mut buf[..total];
    let view = *view;
    let display = *display;
    let vp = *vp;
    // Outside-image (letterbox) background — the theme-aware clear color from the surface.

    let ncpu = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let nthreads = ncpu.min(h as usize).max(1);
    let rows_per = (h as usize + nthreads - 1) / nthreads;
    let band = rows_per * w as usize;

    std::thread::scope(|sc| {
        let mut rest: &mut [u32] = buf;
        let mut y_start: u32 = 0;
        while !rest.is_empty() {
            let take = band.min(rest.len());
            let (chunk, tail) = rest.split_at_mut(take);
            rest = tail;
            let ys = y_start;
            y_start += (take / w as usize) as u32;
            sc.spawn(move || {
                shade_band(chunk, ys, w, img, view, display, &vp, bg, luts);
            });
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn shade_band(
    chunk: &mut [u32],
    y_start: u32,
    w: u32,
    img: &DecodedImage,
    view: ViewState,
    display: DisplayState,
    vp: &Viewport,
    bg: u32,
    luts: &Luts,
) {
    let iw = img.width as f32;
    let ih = img.height as f32;
    let iwi = img.width as i32;
    let ihi = img.height as i32;
    let bpp = bytes_per_pixel(img.format);
    let stride = img.width as usize * bpp;
    let is_hdr = img.format.is_hdr();
    let exposure = display.exposure.exp2();

    let c = vp.center();
    let cx = c.0 + view.pan.0;
    let cy = c.1 + view.pan.1;
    let inv = 1.0 / view.zoom;

    let minify = view.zoom < 1.0;
    let foot = if minify { (inv.round() as i32).clamp(1, MAX_FOOT) } else { 1 };
    let half = foot / 2;
    let inv_taps = 1.0 / (foot * foot) as f32;

    // Fast path is only valid for opaque 8-bit RGB at magnify (sample sRGB == output sRGB).
    let fast8 = img.format == PixelFormat::Rgba8Unorm;

    let rows = chunk.len() / w as usize;
    for yy in 0..rows {
        let y = y_start + yy as u32;
        let sy = y as f32 + 0.5;
        let fy = ih * 0.5 + (sy - cy) * inv;
        let row = yy * w as usize;
        for x in 0..w {
            let sx = x as f32 + 0.5;
            let fx = iw * 0.5 + (sx - cx) * inv;

            let out = if fx < 0.0 || fy < 0.0 || fx >= iw || fy >= ih {
                bg
            } else if !minify {
                let xi = fx as usize;
                let yi = fy as usize;
                // 8-bit fast paths skip the linear round-trip for the common cases.
                if fast8 {
                    let o = yi * stride + xi * 4;
                    let px = &img.pixels;
                    let (r, g, b, a) = (px[o], px[o + 1], px[o + 2], px[o + 3]);
                    match display.channel {
                        Channel::Rgb if a == 255 => pack(r, g, b),
                        Channel::R => pack(r, r, r),
                        Channel::G => pack(g, g, g),
                        Channel::B => pack(b, b, b),
                        Channel::A => pack(a, a, a),
                        Channel::Rgb => {
                            let lin = fetch_linear(img, xi, yi, luts);
                            shade_tail(lin, &display, is_hdr, exposure, x, y, luts)
                        }
                    }
                } else {
                    let lin = fetch_linear(img, xi, yi, luts);
                    shade_tail(lin, &display, is_hdr, exposure, x, y, luts)
                }
            } else {
                // Minify — box-average the source footprint in linear light (all formats).
                let bx = fx as i32;
                let by = fy as i32;
                let mut acc = [0.0f32; 4];
                for dy in 0..foot {
                    let yyy = (by - half + dy).clamp(0, ihi - 1) as usize;
                    for dx in 0..foot {
                        let xxx = (bx - half + dx).clamp(0, iwi - 1) as usize;
                        let s = fetch_linear(img, xxx, yyy, luts);
                        acc[0] += s[0];
                        acc[1] += s[1];
                        acc[2] += s[2];
                        acc[3] += s[3];
                    }
                }
                let lin = [acc[0] * inv_taps, acc[1] * inv_taps, acc[2] * inv_taps, acc[3] * inv_taps];
                shade_tail(lin, &display, is_hdr, exposure, x, y, luts)
            };
            chunk[row + x as usize] = out;
        }
    }
}

/// Fetch one source texel as **linear** RGBA, per pixel format (8-bit & 16-bit unorm are
/// sRGB-encoded → linearized; float is already linear).
#[inline]
fn fetch_linear(img: &DecodedImage, x: usize, y: usize, luts: &Luts) -> [f32; 4] {
    let w = img.width as usize;
    let px = &img.pixels;
    match img.format {
        PixelFormat::Rgba8Unorm => {
            let o = (y * w + x) * 4;
            [
                luts.lin[px[o] as usize],
                luts.lin[px[o + 1] as usize],
                luts.lin[px[o + 2] as usize],
                px[o + 3] as f32 / 255.0,
            ]
        }
        PixelFormat::Rgba16Unorm => {
            let o = (y * w + x) * 8;
            let u = |i: usize| u16::from_ne_bytes([px[o + i], px[o + i + 1]]) as f32 / 65535.0;
            [srgb_to_linear(u(0)), srgb_to_linear(u(2)), srgb_to_linear(u(4)), u(6)]
        }
        PixelFormat::Rgba16Float => {
            let o = (y * w + x) * 8;
            let f = |i: usize| f16_to_f32(u16::from_ne_bytes([px[o + i], px[o + i + 1]]));
            [f(0), f(2), f(4), f(6)]
        }
        PixelFormat::Rgba32Float => {
            let o = (y * w + x) * 16;
            let f = |i: usize| f32::from_ne_bytes([px[o + i], px[o + i + 1], px[o + i + 2], px[o + i + 3]]);
            [f(0), f(4), f(8), f(12)]
        }
    }
}

/// The common linear-light tail: HDR exposure/tonemap → channel isolation → checkerboard
/// composite → sRGB encode. Returns a packed `0x00RRGGBB` pixel.
#[inline]
fn shade_tail(
    lin: [f32; 4],
    display: &DisplayState,
    is_hdr: bool,
    exposure: f32,
    x: u32,
    y: u32,
    luts: &Luts,
) -> u32 {
    let (mut r, mut g, mut b) = (lin[0], lin[1], lin[2]);
    let a = lin[3];

    if is_hdr {
        r *= exposure;
        g *= exposure;
        b *= exposure;
        let tm = match display.tonemap {
            Tonemap::Reinhard => reinhard,
            Tonemap::Aces => aces,
        };
        r = tm(r);
        g = tm(g);
        b = tm(b);
    }

    match display.channel {
        Channel::R => {
            let v = enc(&luts.srgb, r);
            pack(v, v, v)
        }
        Channel::G => {
            let v = enc(&luts.srgb, g);
            pack(v, v, v)
        }
        Channel::B => {
            let v = enc(&luts.srgb, b);
            pack(v, v, v)
        }
        Channel::A => {
            // Alpha shown literally as gray (== sRGB_encode(sRGB_decode(a)) in the shader).
            let v = (a.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
            pack(v, v, v)
        }
        Channel::Rgb => {
            if a < 0.999 {
                let bg = checker(x, y);
                r = bg * (1.0 - a) + r * a;
                g = bg * (1.0 - a) + g * a;
                b = bg * (1.0 - a) + b * a;
            }
            pack(enc(&luts.srgb, r), enc(&luts.srgb, g), enc(&luts.srgb, b))
        }
    }
}

#[inline]
fn bytes_per_pixel(format: PixelFormat) -> usize {
    match format {
        PixelFormat::Rgba8Unorm => 4,
        PixelFormat::Rgba16Unorm | PixelFormat::Rgba16Float => 8,
        PixelFormat::Rgba32Float => 16,
    }
}

#[inline]
fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

#[inline]
fn linear_to_srgb(c: f32) -> f32 {
    if c <= 0.0031308 {
        12.92 * c
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

/// Reinhard tonemap, per component (matches the shader's `c / (1 + c)`).
#[inline]
fn reinhard(c: f32) -> f32 {
    c / (1.0 + c)
}

/// Narkowicz 2015 ACES filmic fit, per component (matches the shader).
#[inline]
fn aces(x: f32) -> f32 {
    let (a, b, c, d, e) = (2.51, 0.03, 2.43, 0.59, 0.14);
    ((x * (a * x + b)) / (x * (c * x + d) + e)).clamp(0.0, 1.0)
}

#[inline]
fn enc(lut_srgb: &[u8], lin: f32) -> u8 {
    let i = (lin.clamp(0.0, 1.0) * 4096.0 + 0.5) as usize;
    lut_srgb[i.min(4096)]
}

#[inline]
const fn pack(r: u8, g: u8, b: u8) -> u32 {
    ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
}

/// Two neutral grays in linear (match the shader), composited then sRGB-encoded.
#[inline]
fn checker(x: u32, y: u32) -> f32 {
    let cx = (x as f32 / CHECKER_SIZE).floor();
    let cy = (y as f32 / CHECKER_SIZE).floor();
    if ((cx + cy) as i64 & 1) == 0 {
        0.45
    } else {
        0.21
    }
}

/// IEEE-754 half (f16) bits → f32. Handles subnormals, inf, and NaN (payload preserved).
#[inline]
fn f16_to_f32(h: u16) -> f32 {
    let sign = (h as u32 & 0x8000) << 16;
    let exp = (h >> 10) & 0x1f;
    let mant = (h & 0x03ff) as u32;
    let bits = if exp == 0 {
        if mant == 0 {
            sign
        } else {
            // Subnormal: normalize into a float32 normal.
            let mut e: i32 = -1;
            let mut m = mant;
            while (m & 0x0400) == 0 {
                m <<= 1;
                e -= 1;
            }
            m &= 0x03ff;
            let fe = (e + 1 + (127 - 15)) as u32;
            sign | (fe << 23) | (m << 13)
        }
    } else if exp == 0x1f {
        sign | 0x7f80_0000 | (mant << 13)
    } else {
        let fe = exp as u32 + (127 - 15);
        sign | (fe << 23) | (mant << 13)
    };
    f32::from_bits(bits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img_8(w: u32, h: u32, pixels: Vec<u8>) -> DecodedImage {
        DecodedImage {
            pixels,
            width: w,
            height: h,
            format: PixelFormat::Rgba8Unorm,
            bit_depth: 8,
            channels: 4,
            icc: None,
            source_format: "TEST",
            downscaled_from: None,
        }
    }

    #[test]
    fn srgb_lut_round_trips() {
        let luts = Luts::new();
        // enc(lin[b]) should recover the original byte for every value.
        for b in 0u16..=255 {
            let recovered = enc(&luts.srgb, luts.lin[b as usize]);
            assert!(
                (recovered as i16 - b as i16).abs() <= 1,
                "byte {b} round-tripped to {recovered}"
            );
        }
    }

    #[test]
    fn fetch_linear_8bit_decodes_srgb() {
        let luts = Luts::new();
        // A single mid-gray opaque pixel: 188 sRGB ≈ 0.5 linear.
        let img = img_8(1, 1, vec![188, 188, 188, 255]);
        let s = fetch_linear(&img, 0, 0, &luts);
        assert!((s[0] - 0.5).abs() < 0.02, "linear {} not ~0.5", s[0]);
        assert_eq!(s[3], 1.0);
    }

    #[test]
    fn f16_round_numbers() {
        assert_eq!(f16_to_f32(0x3C00), 1.0);
        assert_eq!(f16_to_f32(0x4000), 2.0);
        assert_eq!(f16_to_f32(0x3800), 0.5);
        assert_eq!(f16_to_f32(0x0000), 0.0);
        assert_eq!(f16_to_f32(0xBC00), -1.0);
    }

    #[test]
    fn opaque_rgb_magnify_is_passthrough() {
        // At 1:1, an opaque 8-bit pixel should reach the framebuffer byte-exact.
        let luts = Luts::new();
        let img = img_8(2, 2, vec![
            10, 20, 30, 255, 40, 50, 60, 255, //
            70, 80, 90, 255, 100, 110, 120, 255,
        ]);
        let mut buf = vec![0u32; 4];
        let mut view = ViewState::default();
        view.zoom = 1.0;
        view.fit = false;
        let display = DisplayState::default();
        let vp = Viewport::new(2, 2);
        shade(&mut buf, 2, 2, &img, &view, &display, &vp, &luts, 0);
        // Top-left surface pixel maps to the image's top-left texel (10,20,30).
        assert_eq!(buf[0], pack(10, 20, 30));
    }

    #[test]
    fn solo_alpha_shows_coverage_gray() {
        let luts = Luts::new();
        let img = img_8(1, 1, vec![255, 0, 0, 128]); // half-transparent red
        let mut buf = vec![0u32; 1];
        let mut view = ViewState::default();
        view.zoom = 1.0;
        view.fit = false;
        let mut display = DisplayState::default();
        display.channel = Channel::A;
        let vp = Viewport::new(1, 1);
        shade(&mut buf, 1, 1, &img, &view, &display, &vp, &luts, 0);
        assert_eq!(buf[0], pack(128, 128, 128));
    }
}
