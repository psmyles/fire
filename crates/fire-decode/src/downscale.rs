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
    let longest = src_w.max(src_h) as f64;
    let scale = max_dim as f64 / longest;
    let new_w = ((src_w as f64 * scale).floor() as u32).clamp(1, max_dim);
    let new_h = ((src_h as f64 * scale).floor() as u32).clamp(1, max_dim);

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
