//! Decoded image → thumbnail bitmap, on the CPU.
//!
//! The viewer's per-pixel pipeline, run once per output pixel instead of per screen pixel: sample in
//! linear light, then exposure → tonemap (HDR only) → composite over the backdrop → sRGB-encode —
//! the same order and the same curves as `fire`'s `render/shader.glsl`, so a thumbnail looks like
//! the image does in Fire zoomed out.
//!
//! "Sample" here is an area average: each output pixel is the mean of every source pixel under it,
//! with partial weights at the edges — a box filter that is exact for any ratio, not just halvings.
//! It is taken in **linear light** (as the viewer's mip chain is) and on **premultiplied** colour,
//! so the RGB a file leaves under fully transparent pixels cannot bleed into the edges of a sprite.
//!
//! The output is what a 32-bit DIB section holds: top-down BGRA8 with *straight* alpha. Straight,
//! not premultiplied, because that is what Explorer's own photo thumbnailer hands back — measured,
//! not assumed: a 50%-transparent red PNG comes back through the shell as `(0,0,255,128)`.

use std::sync::OnceLock;

use fire_decode::{DecodedImage, PixelFormat};

use crate::settings::{Backdrop, ThumbnailsCfg, Tonemap};

/// A finished thumbnail.
#[derive(Debug, Clone, PartialEq)]
pub struct Thumbnail {
    pub width: u32,
    pub height: u32,
    /// `width × height` pixels, top row first, BGRA8, straight alpha.
    pub bgra: Vec<u8>,
    /// Whether any pixel is less than opaque — the difference between telling Explorer to blend it
    /// (`WTSAT_ARGB`) and to draw it as is (`WTSAT_RGB`). Never set over an opaque backdrop.
    pub has_alpha: bool,
}

/// The size a `w`×`h` image is drawn at in a `cx`-pixel thumbnail: long side scaled to `cx`, aspect
/// kept, neither side below 1. Never enlarged — Explorer shows a 16×16 icon at 16×16, the way it
/// does its own small images, rather than as a blur of a few giant pixels.
pub fn fit(w: u32, h: u32, cx: u32) -> (u32, u32) {
    let (w, h, cx) = (w.max(1), h.max(1), cx.max(1));
    let long = w.max(h);
    if long <= cx {
        return (w, h);
    }
    let s = cx as f64 / long as f64;
    let scale = |v: u32| ((v as f64 * s).round() as u32).clamp(1, cx);
    (scale(w), scale(h))
}

/// Draw `img` as a thumbnail at most `cx` pixels on its long side. `None` when the image has no
/// pixels or its buffer is shorter than its dimensions say — a decoder bug, not a picture.
///
/// Draws the canvas: an animated GIF's first frame, a DDS cubemap's whole sheet of faces — what the
/// viewer opens on.
pub fn render(img: &DecodedImage, cx: u32, cfg: &ThumbnailsCfg) -> Option<Thumbnail> {
    let (sw, sh) = (img.width as usize, img.height as usize);
    let needed = sw
        .checked_mul(sh)?
        .checked_mul(img.format.bytes_per_pixel())?;
    if sw == 0 || sh == 0 || img.pixels.len() < needed {
        return None;
    }
    let (w, h) = fit(img.width, img.height, cx);
    let linear = resample(img, w as usize, h as usize);

    let hdr = img.format.is_hdr();
    let gain = cfg.exposure.exp2();
    // The checkerboard is drawn at a fixed count of cells across the long side rather than the
    // viewer's 12 screen pixels: Explorer shrinks this one bitmap for every view smaller than the
    // one that asked for it, and a fixed pixel size would turn to mush in the small ones.
    let cell = (w.max(h) as usize / 16).max(2);

    let mut bgra = Vec::with_capacity(linear.len() * 4);
    let mut has_alpha = false;
    for (i, p) in linear.iter().enumerate() {
        let a = p[3].clamp(0.0, 1.0);
        let mut rgb = if a > 0.0 {
            [p[0] / a, p[1] / a, p[2] / a]
        } else {
            [0.0; 3]
        };
        if hdr {
            rgb = tonemap(cfg.tonemap, rgb.map(|c| c * gain));
        }
        let (rgb, a) = match backdrop(cfg.backdrop, i % w as usize, i / w as usize, cell) {
            Some(bd) => (std::array::from_fn(|c| bd[c] * (1.0 - a) + rgb[c] * a), 1.0),
            None => (rgb, a),
        };
        let a8 = (a * 255.0).round() as u8;
        if a8 == 0 {
            // Nothing to see: zero the colour too, so no consumer that ignores alpha finds garbage.
            bgra.extend_from_slice(&[0; 4]);
        } else {
            bgra.extend_from_slice(&[encode(rgb[2]), encode(rgb[1]), encode(rgb[0]), a8]);
        }
        has_alpha |= a8 < 255;
    }
    Some(Thumbnail {
        width: w,
        height: h,
        bgra,
        has_alpha,
    })
}

/// Linear-light RGBA, premultiplied — the space every average is taken in.
type Px = [f32; 4];

/// The backdrop's linear colour at output pixel `(x, y)`, `None` for real transparency. The
/// viewer's values: 40% sRGB grey, and a 0.45/0.21 checker.
fn backdrop(b: Backdrop, x: usize, y: usize, cell: usize) -> Option<[f32; 3]> {
    let v = match b {
        Backdrop::Transparent => return None,
        Backdrop::Black => 0.0,
        Backdrop::White => 1.0,
        Backdrop::Grey => srgb_to_linear(0.4),
        Backdrop::Checker if (x / cell + y / cell).is_multiple_of(2) => 0.45,
        Backdrop::Checker => 0.21,
    };
    Some([v; 3])
}

fn tonemap(t: Tonemap, c: [f32; 3]) -> [f32; 3] {
    match t {
        Tonemap::Reinhard => c.map(|x| x / (1.0 + x)),
        Tonemap::Aces => c.map(|x| {
            let (a, b, c, d, e) = (2.51, 0.03, 2.43, 0.59, 0.14);
            ((x * (a * x + b)) / (x * (c * x + d) + e)).clamp(0.0, 1.0)
        }),
    }
}

fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Linear → sRGB, quantized to 8 bits.
fn encode(c: f32) -> u8 {
    let c = c.clamp(0.0, 1.0);
    let s = if c <= 0.003_130_8 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    };
    (s * 255.0).round() as u8
}

/// Every 8-bit sRGB code, in linear light.
fn srgb8_lut() -> &'static [f32; 256] {
    static LUT: OnceLock<[f32; 256]> = OnceLock::new();
    LUT.get_or_init(|| std::array::from_fn(|i| srgb_to_linear(i as f32 / 255.0)))
}

/// Every 16-bit sRGB code, in linear light. 256 KiB, built once per process: a 16-bit PSD or PNG
/// would otherwise cost a `powf` per channel per source pixel.
fn srgb16_lut() -> &'static [f32] {
    static LUT: OnceLock<Vec<f32>> = OnceLock::new();
    LUT.get_or_init(|| {
        (0..=u16::MAX)
            .map(|i| srgb_to_linear(i as f32 / 65535.0))
            .collect()
    })
}

/// A float sample fit to average: NaN and negatives to 0, infinity to the largest half float. An
/// HDR file with one bad texel must not turn its whole neighbourhood of the thumbnail black.
fn clean(v: f32) -> f32 {
    if v.is_nan() {
        0.0
    } else {
        v.clamp(0.0, 65504.0)
    }
}

/// Float alpha into `0..=1`. NaN counts as opaque, as it does in the viewer's composite.
fn clean_alpha(a: f32) -> f32 {
    if a.is_nan() {
        1.0
    } else {
        a.clamp(0.0, 1.0)
    }
}

/// Source row `y` of `img`, in linear light, premultiplied.
fn load_row(img: &DecodedImage, y: usize, out: &mut [Px]) {
    let w = img.width as usize;
    let bpp = img.format.bytes_per_pixel();
    let row = &img.pixels[y * w * bpp..(y + 1) * w * bpp];
    let px = |rgb: [f32; 3], a: f32| [rgb[0] * a, rgb[1] * a, rgb[2] * a, a];
    match img.format {
        PixelFormat::Rgba8Unorm => {
            let lut = srgb8_lut();
            for (o, p) in out.iter_mut().zip(row.as_chunks::<4>().0) {
                let rgb = [lut[p[0] as usize], lut[p[1] as usize], lut[p[2] as usize]];
                *o = px(rgb, p[3] as f32 / 255.0);
            }
        }
        PixelFormat::Rgba16Unorm => {
            let lut = srgb16_lut();
            for (o, p) in out.iter_mut().zip(row.as_chunks::<8>().0) {
                let c = |i: usize| u16::from_ne_bytes([p[2 * i], p[2 * i + 1]]);
                let rgb = [lut[c(0) as usize], lut[c(1) as usize], lut[c(2) as usize]];
                *o = px(rgb, c(3) as f32 / 65535.0);
            }
        }
        PixelFormat::Rgba16Float => {
            for (o, p) in out.iter_mut().zip(row.as_chunks::<8>().0) {
                let c = |i: usize| half::f16::from_ne_bytes([p[2 * i], p[2 * i + 1]]).to_f32();
                *o = px([clean(c(0)), clean(c(1)), clean(c(2))], clean_alpha(c(3)));
            }
        }
        PixelFormat::Rgba32Float => {
            for (o, p) in out.iter_mut().zip(row.as_chunks::<16>().0) {
                let c = |i: usize| {
                    f32::from_ne_bytes([p[4 * i], p[4 * i + 1], p[4 * i + 2], p[4 * i + 3]])
                };
                *o = px([clean(c(0)), clean(c(1)), clean(c(2))], clean_alpha(c(3)));
            }
        }
    }
}

/// Which source samples one output sample on an axis averages, and how much of each: for output
/// `i`, the samples from `first[i]` on, weighted by `weights[i]` (which sum to 1).
#[derive(Debug)]
struct Taps {
    first: Vec<usize>,
    weights: Vec<Vec<f32>>,
}

/// The box filter shrinking a `src`-long axis to `dst`: output `i` covers source
/// `[i·src/dst, (i+1)·src/dst)`, and each source sample weighs what of it falls inside. At
/// `src == dst` it is one sample at weight 1, so the no-resize case runs the same code.
fn taps(src: usize, dst: usize) -> Taps {
    let scale = src as f64 / dst as f64;
    let mut first = Vec::with_capacity(dst);
    let mut weights = Vec::with_capacity(dst);
    for i in 0..dst {
        let lo = i as f64 * scale;
        let hi = ((i + 1) as f64 * scale).min(src as f64);
        let a = (lo.floor() as usize).min(src - 1);
        let b = (hi.ceil() as usize).clamp(a + 1, src);
        let mut w: Vec<f32> = (a..b)
            .map(|s| (hi.min(s as f64 + 1.0) - lo.max(s as f64)).max(0.0) as f32)
            .collect();
        let sum: f32 = w.iter().sum();
        if sum > 0.0 {
            w.iter_mut().for_each(|x| *x /= sum);
        }
        first.push(a);
        weights.push(w);
    }
    Taps { first, weights }
}

/// Below this many source pixels the whole resample runs on the calling thread: the hand-off would
/// cost more than the work.
const PARALLEL_MIN_PIXELS: usize = 1 << 20;

/// Area-average `img` down to `w`×`h`, in linear premultiplied light.
fn resample(img: &DecodedImage, w: usize, h: usize) -> Vec<Px> {
    let (sw, sh) = (img.width as usize, img.height as usize);
    let tx = taps(sw, w);
    let ty = taps(sh, h);
    let mut out = vec![[0.0; 4]; w * h];

    let threads = if sw * sh >= PARALLEL_MIN_PIXELS {
        std::thread::available_parallelism().map_or(1, |n| n.get().min(8))
    } else {
        1
    };
    // Bands of output rows, one per thread. Each band reads only the source rows under it, so a
    // source row straddling two bands is converted twice — one row per band, against a wholly
    // lock-free split.
    let rows_per = h.div_ceil(threads.clamp(1, h));
    if threads <= 1 {
        fill_band(img, &tx, &ty, 0, &mut out);
    } else {
        std::thread::scope(|s| {
            for (i, band) in out.chunks_mut(rows_per * w).enumerate() {
                let (tx, ty) = (&tx, &ty);
                s.spawn(move || fill_band(img, tx, ty, i * rows_per, band));
            }
        });
    }
    out
}

/// Fill output rows `j0..` (`band`, whole rows) from the source rows under them. Source rows are
/// read in order, and the one shared between two consecutive output rows is converted once.
fn fill_band(img: &DecodedImage, tx: &Taps, ty: &Taps, j0: usize, band: &mut [Px]) {
    let w = tx.first.len();
    let mut src = vec![[0.0; 4]; img.width as usize];
    let mut row = vec![[0.0; 4]; w];
    let mut loaded = None;
    for (dj, out) in band.chunks_mut(w).enumerate() {
        let j = j0 + dj;
        for (k, &wy) in ty.weights[j].iter().enumerate() {
            let y = ty.first[j] + k;
            if loaded != Some(y) {
                load_row(img, y, &mut src);
                for (i, r) in row.iter_mut().enumerate() {
                    let mut acc = [0.0f32; 4];
                    for (n, &wx) in tx.weights[i].iter().enumerate() {
                        let p = src[tx.first[i] + n];
                        for c in 0..4 {
                            acc[c] += p[c] * wx;
                        }
                    }
                    *r = acc;
                }
                loaded = Some(y);
            }
            for (o, p) in out.iter_mut().zip(&row) {
                for c in 0..4 {
                    o[c] += p[c] * wy;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(w: u32, h: u32, format: PixelFormat, pixels: Vec<u8>) -> DecodedImage {
        DecodedImage {
            pixels,
            width: w,
            height: h,
            format,
            bit_depth: 8,
            channels: 4,
            alpha_opaque: false,
            icc: None,
            source_format: "test",
            downscaled_from: None,
            source_mips: None,
            layout: None,
            animation: None,
        }
    }

    fn rgba8(w: u32, h: u32, f: impl Fn(u32, u32) -> [u8; 4]) -> DecodedImage {
        let mut px = Vec::new();
        for y in 0..h {
            for x in 0..w {
                px.extend_from_slice(&f(x, y));
            }
        }
        image(w, h, PixelFormat::Rgba8Unorm, px)
    }

    fn at(t: &Thumbnail, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * t.width + x) * 4) as usize;
        t.bgra[i..i + 4].try_into().unwrap()
    }

    #[test]
    fn fit_scales_the_long_side_and_never_enlarges() {
        assert_eq!(fit(1024, 512, 256), (256, 128));
        assert_eq!(fit(512, 1024, 256), (128, 256));
        assert_eq!(fit(16, 16, 256), (16, 16));
        assert_eq!(fit(10_000, 1, 256), (256, 1)); // never 0 on the short side
        assert_eq!(fit(300, 299, 256), (256, 255));
    }

    /// At 1:1 every 8-bit code survives the trip into linear light and back out.
    #[test]
    fn unscaled_8bit_is_lossless() {
        let img = rgba8(256, 1, |x, _| [x as u8, 255 - x as u8, (x * 7) as u8, 255]);
        let t = render(&img, 256, &ThumbnailsCfg::default()).unwrap();
        assert_eq!((t.width, t.height), (256, 1));
        assert!(!t.has_alpha);
        for x in 0..256 {
            let [b, g, r, a] = at(&t, x, 0);
            assert_eq!(
                [r, g, b, a],
                [x as u8, 255 - x as u8, (x * 7) as u8, 255],
                "x={x}"
            );
        }
    }

    /// Black and white columns average to the *linear* midpoint — sRGB 188, not 128 — which is
    /// what keeps a fine pattern from darkening when it shrinks.
    #[test]
    fn averages_in_linear_light() {
        let img = rgba8(512, 2, |x, _| {
            if x % 2 == 0 {
                [0, 0, 0, 255]
            } else {
                [255, 255, 255, 255]
            }
        });
        let t = render(&img, 256, &ThumbnailsCfg::default()).unwrap();
        assert_eq!((t.width, t.height), (256, 1));
        assert_eq!(at(&t, 100, 0), [188, 188, 188, 255]);
    }

    /// A shrink by a non-integer ratio still gives every source pixel the same total weight: a
    /// uniform image stays uniform, edge columns included.
    #[test]
    fn fractional_ratios_stay_uniform() {
        let img = rgba8(1000, 700, |_, _| [200, 100, 50, 255]);
        let t = render(&img, 256, &ThumbnailsCfg::default()).unwrap();
        assert_eq!((t.width, t.height), (256, 179));
        for y in [0, 90, 178] {
            for x in [0, 128, 255] {
                assert_eq!(at(&t, x, y), [50, 100, 200, 255], "({x},{y})");
            }
        }
    }

    /// The colour under fully transparent pixels does not bleed: a red sprite on a transparent
    /// field whose hidden colour is green shrinks to red at half coverage, not to a brown.
    #[test]
    fn premultiplied_averaging_hides_transparent_colour() {
        let img = rgba8(2, 1, |x, _| {
            if x == 0 {
                [255, 0, 0, 255]
            } else {
                [0, 255, 0, 0]
            }
        });
        let t = render(&img, 1, &ThumbnailsCfg::default()).unwrap();
        assert!(t.has_alpha);
        assert_eq!(at(&t, 0, 0), [0, 0, 255, 128]); // straight alpha: full red, half covered
    }

    #[test]
    fn backdrops_composite_and_report_opaque() {
        let img = rgba8(4, 4, |_, _| [255, 255, 255, 0]);
        let px = |b| {
            let cfg = ThumbnailsCfg {
                backdrop: b,
                ..Default::default()
            };
            let t = render(&img, 4, &cfg).unwrap();
            (at(&t, 0, 0), t.has_alpha)
        };
        assert_eq!(px(Backdrop::Transparent), ([0, 0, 0, 0], true));
        assert_eq!(px(Backdrop::Black), ([0, 0, 0, 255], false));
        assert_eq!(px(Backdrop::White), ([255, 255, 255, 255], false));
        assert_eq!(px(Backdrop::Grey), ([102, 102, 102, 255], false)); // sRGB 0.4
        let (c, a) = px(Backdrop::Checker);
        assert!(!a);
        assert_eq!(c[3], 255);
    }

    /// HDR goes through exposure and the tonemap; 1.0 linear under Reinhard is 0.5 linear, sRGB 188.
    /// One stop up is 2.0 → 2/3.
    #[test]
    fn hdr_is_exposed_and_tonemapped() {
        let one: Vec<u8> = [1.0f32, 1.0, 1.0, 1.0]
            .iter()
            .flat_map(|v| v.to_ne_bytes())
            .collect();
        let img = image(1, 1, PixelFormat::Rgba32Float, one);
        let t = render(&img, 1, &ThumbnailsCfg::default()).unwrap();
        assert_eq!(at(&t, 0, 0), [188, 188, 188, 255]);
        let up = ThumbnailsCfg {
            exposure: 1.0,
            ..Default::default()
        };
        let t = render(&img, 1, &up).unwrap();
        assert_eq!(at(&t, 0, 0)[0], encode(2.0 / 3.0));
        let aces = ThumbnailsCfg {
            tonemap: Tonemap::Aces,
            ..Default::default()
        };
        let t = render(&img, 1, &aces).unwrap();
        assert_eq!(at(&t, 0, 0)[0], encode(tonemap(Tonemap::Aces, [1.0; 3])[0]));
    }

    /// One NaN or infinite texel in a float image costs its own pixel, not the average around it.
    #[test]
    fn bad_float_texels_do_not_poison_the_average() {
        let mut px = Vec::new();
        for v in [f32::NAN, 1.0, f32::INFINITY, 1.0] {
            for c in [v, v, v, 1.0] {
                px.extend_from_slice(&half::f16::from_f32(c).to_ne_bytes());
            }
        }
        let img = image(4, 1, PixelFormat::Rgba16Float, px);
        let t = render(&img, 1, &ThumbnailsCfg::default()).unwrap();
        let [b, g, r, a] = at(&t, 0, 0);
        assert_eq!(a, 255);
        assert!(r == g && g == b && r > 0, "{r}");
    }

    #[test]
    fn sixteen_bit_matches_eight_bit() {
        let px: Vec<u8> = [0x8080u16, 0x4040, 0xffff, 0xffff]
            .iter()
            .flat_map(|v| v.to_ne_bytes())
            .collect();
        let img = image(1, 1, PixelFormat::Rgba16Unorm, px);
        let t = render(&img, 1, &ThumbnailsCfg::default()).unwrap();
        assert_eq!(at(&t, 0, 0), [255, 0x40, 0x80, 255]);
    }

    /// The threaded path (a source over a megapixel) gives the same pixels as the serial one.
    #[test]
    fn banded_resample_matches_serial() {
        let img = rgba8(1500, 900, |x, y| {
            [(x % 251) as u8, (y % 241) as u8, ((x ^ y) % 256) as u8, 255]
        });
        let (w, h) = fit(1500, 900, 200);
        let par = resample(&img, w as usize, h as usize);
        let mut serial = vec![[0.0; 4]; (w * h) as usize];
        fill_band(
            &img,
            &taps(1500, w as usize),
            &taps(900, h as usize),
            0,
            &mut serial,
        );
        assert_eq!(par, serial);
    }

    #[test]
    fn short_buffers_are_refused() {
        let img = image(4, 4, PixelFormat::Rgba8Unorm, vec![0; 15]);
        assert!(render(&img, 4, &ThumbnailsCfg::default()).is_none());
    }
}
