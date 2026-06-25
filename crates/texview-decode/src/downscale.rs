//! CPU downscale-to-fit. Anything past the GPU max texture dimension (§6) is shrunk to
//! fit before upload, recording the original size so the pixel inspector can note that a
//! read came from the downscaled copy. Nearest-neighbor: this path only triggers for
//! images larger than ~16384px on an axis (rare), where avoiding a crash matters more
//! than filter quality; tiled/virtual texturing for gigapixel images is a v2 item.

use crate::DecodedImage;

/// Shrink `img` in place so neither dimension exceeds `max_dim`. No-op if it already fits.
pub fn to_fit(img: &mut DecodedImage, max_dim: u32) {
    if max_dim == 0 || (img.width <= max_dim && img.height <= max_dim) {
        return;
    }

    let longest = img.width.max(img.height) as f64;
    let scale = max_dim as f64 / longest;
    let new_w = ((img.width as f64 * scale).floor() as u32).clamp(1, max_dim);
    let new_h = ((img.height as f64 * scale).floor() as u32).clamp(1, max_dim);

    let bpp = img.format.bytes_per_pixel();
    let src_w = img.width as usize;
    let mut out = vec![0u8; new_w as usize * new_h as usize * bpp];

    for y in 0..new_h as usize {
        // Map destination row to the nearest source row.
        let sy = (y as u64 * img.height as u64 / new_h as u64) as usize;
        for x in 0..new_w as usize {
            let sx = (x as u64 * img.width as u64 / new_w as u64) as usize;
            let src = (sy * src_w + sx) * bpp;
            let dst = (y * new_w as usize + x) * bpp;
            out[dst..dst + bpp].copy_from_slice(&img.pixels[src..src + bpp]);
        }
    }

    img.downscaled_from = Some((img.width, img.height));
    img.pixels = out;
    img.width = new_w;
    img.height = new_h;
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
            downscaled_from: None,
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
