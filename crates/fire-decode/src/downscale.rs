//! CPU downscale-to-fit. Anything past the caller's `max_dim` (§6) is shrunk to fit,
//! recording the original size so the pixel inspector can note that a read came from the
//! downscaled copy. Nearest-neighbor: this path only triggers for images larger than the
//! cap (~16384px on an axis by default; rare), where bounding memory matters more than
//! filter quality; tiled/virtual texturing for gigapixel images is a v2 item.

use crate::DecodedImage;

/// Shrink `img` in place so neither dimension exceeds `max_dim`. No-op if it already fits. For an
/// animated image every frame is resampled to the new canvas, so the whole animation stays
/// consistent (this path is rare for GIFs — they seldom exceed the cap — but must not desync).
pub fn to_fit(img: &mut DecodedImage, max_dim: u32) {
    if max_dim == 0 || (img.width <= max_dim && img.height <= max_dim) {
        return;
    }

    let (src_w, src_h) = (img.width, img.height);
    let (new_w, new_h) = fit_dims(src_w, src_h, max_dim);

    let bpp = img.format.bytes_per_pixel();
    // Same posture as `exif::apply`: never index past a buffer shorter than its declared
    // dimensions. A short canvas leaves the image at full size (shown un-shrunk rather than
    // panicking the decode); `transform_buffers` drops any short frame, because after the
    // canvas resamples, every surviving frame must match the new size or the player reads
    // out of bounds.
    let Some(needed) = (src_w as usize)
        .checked_mul(src_h as usize)
        .and_then(|n| n.checked_mul(bpp))
    else {
        return;
    };
    if img.pixels.len() < needed {
        return;
    }
    img.transform_buffers(needed, |pixels| {
        *pixels = resample(pixels, src_w, src_h, new_w, new_h, bpp);
    });

    img.downscaled_from = Some((src_w, src_h));
    img.width = new_w;
    img.height = new_h;
}

/// The size `to_fit` shrinks a `w×h` image to when it exceeds `max_dim` on an axis.
pub fn fit_dims(w: u32, h: u32, max_dim: u32) -> (u32, u32) {
    if max_dim == 0 || (w <= max_dim && h <= max_dim) {
        return (w, h);
    }
    let scale = max_dim as f64 / w.max(h) as f64;
    (
        ((w as f64 * scale).floor() as u32).clamp(1, max_dim),
        ((h as f64 * scale).floor() as u32).clamp(1, max_dim),
    )
}

/// Downscale a buffer that covers only part of a canvas — a PSD layer's on-canvas bounds at
/// `origin` — exactly as [`to_fit`] would downscale the whole canvas, so the shrunken layer lands
/// on the same pixels as the shrunken composite. Returns the displayed canvas size and the
/// buffer's new origin on it. The destination columns kept are those whose nearest-neighbour
/// source column (`x' * W / nw`, the mapping `resample` uses) falls inside the buffer; likewise
/// rows. A layer that shrinks to nothing comes back as a 1×1 transparent pixel.
pub fn rect_to_fit(
    img: &mut DecodedImage,
    canvas: (u32, u32),
    origin: (u32, u32),
    max_dim: u32,
) -> ((u32, u32), (u32, u32)) {
    let (cw, ch) = canvas;
    let (nw, nh) = fit_dims(cw, ch, max_dim);
    if (nw, nh) == (cw, ch) {
        return (canvas, origin);
    }
    // Destination range [ceil(a·n/N), ceil(b·n/N)) for a source span [a, b).
    let span = |a: u32, len: u32, n: u32, full: u32| {
        let lo = (a as u64 * n as u64).div_ceil(full as u64);
        let hi = ((a + len) as u64 * n as u64).div_ceil(full as u64);
        (lo as u32, hi.saturating_sub(lo) as u32)
    };
    let (x0, w) = span(origin.0, img.width, nw, cw);
    let (y0, h) = span(origin.1, img.height, nh, ch);
    let bpp = img.format.bytes_per_pixel();
    let src_w = img.width as usize;
    let mut out = vec![0u8; (w.max(1) as usize) * (h.max(1) as usize) * bpp];
    if w > 0 && h > 0 {
        for y in 0..h as usize {
            let sy = ((y0 as u64 + y as u64) * ch as u64 / nh as u64) as usize - origin.1 as usize;
            for x in 0..w as usize {
                let sx =
                    ((x0 as u64 + x as u64) * cw as u64 / nw as u64) as usize - origin.0 as usize;
                let s = (sy * src_w + sx) * bpp;
                let d = (y * w as usize + x) * bpp;
                out[d..d + bpp].copy_from_slice(&img.pixels[s..s + bpp]);
            }
        }
    }
    img.downscaled_from = Some((img.width, img.height));
    img.pixels = out;
    img.width = w.max(1);
    img.height = h.max(1);
    img.source_mips = None;
    ((nw, nh), (x0.min(nw - 1), y0.min(nh - 1)))
}

/// Nearest-neighbor resample of one interleaved `bpp`-byte-per-pixel buffer from `src_w×src_h` to
/// `new_w×new_h`. Shared by the main image and each animation frame.
fn resample(src: &[u8], src_w: u32, src_h: u32, new_w: u32, new_h: u32, bpp: usize) -> Vec<u8> {
    let src_w_us = src_w as usize;
    let mut out = vec![0u8; new_w as usize * new_h as usize * bpp];
    for y in 0..new_h as usize {
        // Map destination row to the nearest source row.
        let sy = (y as u64 * src_h as u64 / new_h as u64) as usize;
        for x in 0..new_w as usize {
            let sx = (x as u64 * src_w as u64 / new_w as u64) as usize;
            let s = (sy * src_w_us + sx) * bpp;
            let d = (y * new_w as usize + x) * bpp;
            out[d..d + bpp].copy_from_slice(&src[s..s + bpp]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PixelFormat;

    fn solid(w: u32, h: u32) -> DecodedImage {
        DecodedImage {
            pixels: vec![0xAB; (w * h * 4) as usize],
            width: w,
            height: h,
            format: PixelFormat::Rgba8Unorm,
            bit_depth: 8,
            channels: 4,
            icc: None,
            source_format: "test",
            alpha_opaque: false,
            downscaled_from: None,
            source_mips: None,
            layout: None,
            animation: None,
        }
    }

    /// A buffer shorter than its declared dimensions must degrade, never index out of bounds.
    /// The canvas being short leaves the whole image untouched; a short animation frame is
    /// dropped while the healthy ones still shrink with the canvas.
    #[test]
    fn short_buffers_degrade_instead_of_panicking() {
        // Canvas 1 byte short of what 20000x10000 RGBA declares: left at full size.
        let mut img = solid(20000, 10000);
        img.pixels.pop();
        to_fit(&mut img, 16384);
        assert_eq!((img.width, img.height), (20000, 10000));
        assert_eq!(img.downscaled_from, None);

        // Healthy canvas, one short frame among two: the short one is dropped, the other
        // resampled to the new size.
        let mut img = solid(20000, 10000);
        let full = (20000u64 * 10000 * 4) as usize;
        img.animation = Some(crate::Animation {
            frames: vec![
                crate::AnimationFrame {
                    pixels: vec![0xCD; full],
                    delay_ms: 10,
                },
                crate::AnimationFrame {
                    pixels: vec![0xEF; full - 1],
                    delay_ms: 10,
                },
            ],
        });
        to_fit(&mut img, 16384);
        assert_eq!((img.width, img.height), (16384, 8192));
        let anim = img.animation.as_ref().expect("healthy frame survives");
        assert_eq!(anim.frames.len(), 1);
        assert_eq!(
            anim.frames[0].pixels.len(),
            (img.width * img.height * 4) as usize
        );

        // Every frame short: the animation is dropped entirely, the canvas still shrinks.
        let mut img = solid(20000, 10000);
        img.animation = Some(crate::Animation {
            frames: vec![crate::AnimationFrame {
                pixels: vec![0xEF; 4],
                delay_ms: 10,
            }],
        });
        to_fit(&mut img, 16384);
        assert_eq!((img.width, img.height), (16384, 8192));
        assert!(img.animation.is_none());
    }

    /// A layer cut out of a canvas and shrunk with `rect_to_fit` is pixel-for-pixel the same
    /// region of the canvas shrunk with `to_fit`, wherever the layer sits.
    #[test]
    fn rect_to_fit_matches_the_whole_canvas_downscale() {
        let (cw, ch) = (997u32, 613u32);
        let mut canvas = solid(cw, ch);
        for (i, px) in canvas.pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            px.copy_from_slice(&(i as u32 + 1).to_le_bytes()); // never all-zero
        }
        let mut whole = canvas.clone();
        to_fit(&mut whole, 300);
        let (nw, nh) = (whole.width, whole.height);
        let mut seed = 12345u64;
        let mut rnd = |n: u32| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((seed >> 33) % n as u64) as u32
        };
        for _ in 0..200 {
            let (l, t) = (rnd(cw), rnd(ch));
            let (w, h) = (1 + rnd(cw - l), 1 + rnd(ch - t));
            let mut layer = solid(w, h);
            for y in 0..h {
                for x in 0..w {
                    let s = (((t + y) * cw + l + x) * 4) as usize;
                    let d = ((y * w + x) * 4) as usize;
                    layer.pixels[d..d + 4].copy_from_slice(&canvas.pixels[s..s + 4]);
                }
            }
            let (dims, (ox, oy)) = rect_to_fit(&mut layer, (cw, ch), (l, t), 300);
            assert_eq!(dims, (nw, nh));
            if layer.downscaled_from.is_some() && layer.pixels.iter().all(|&b| b == 0) {
                continue; // shrank to nothing: the 1x1 transparent stand-in
            }
            for y in 0..layer.height {
                for x in 0..layer.width {
                    let d = ((y * layer.width + x) * 4) as usize;
                    let s = (((oy + y) * nw + ox + x) * 4) as usize;
                    assert_eq!(layer.pixels[d..d + 4], whole.pixels[s..s + 4]);
                }
            }
        }
    }

    #[test]
    fn no_op_when_within_limit() {
        let mut img = solid(100, 80);
        to_fit(&mut img, 16384);
        assert_eq!((img.width, img.height), (100, 80));
        assert_eq!(img.downscaled_from, None);
    }

    #[test]
    fn shrinks_to_fit_preserving_aspect() {
        let mut img = solid(20000, 10000);
        to_fit(&mut img, 16384);
        assert!(img.width <= 16384 && img.height <= 16384);
        // 2:1 aspect preserved.
        assert_eq!(img.width, 16384);
        assert_eq!(img.height, 8192);
        assert_eq!(img.downscaled_from, Some((20000, 10000)));
        assert_eq!(img.pixels.len(), (img.width * img.height * 4) as usize);
    }

    #[test]
    fn tall_image_clamps_height() {
        let mut img = solid(8000, 40000);
        to_fit(&mut img, 16384);
        assert_eq!(img.height, 16384);
        assert!(img.width <= 16384);
    }
}
