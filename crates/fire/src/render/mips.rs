//! The image's mip chain: what fire keeps of it.
//!
//! sokol_gfx has no `GenerateMips` and its rules rule out building the pyramid in place on the
//! GPU: an image that is rendered into cannot be created with data, and an image cannot be bound
//! as a texture in the pass that renders into it. So the chain is what sokol's documentation says
//! it should be — supplied by the application, every level as data, on the immutable image. It is
//! built on the decode worker, right after the decode and before the image is posted, so the
//! UI thread never pays for it — and while level 0 is being copied to the GPU, where the backend
//! allows (see [`crate::render::gpu::Uploader`]).
//!
//! The builder itself is fire-decode's (its `mips` feature, FD11: [`fire_decode::mips`]), which
//! 3D Review shares: a 2×2 box filter per level, 8-bit sRGB averaged in linear light, a DDS's own
//! levels adopted, the big levels split across the process-wide thread budget. What stays here is
//! the upload side: how many levels a texture has, how wide a texel is, and the conversions for a
//! device that lacks the decoder's format.

pub use fire_decode::mips::level_count;
// The halving rule lives with the decode contract that states it (`DecodedImage::source_mips`).
pub use fire_decode::level_dims;
use fire_decode::PixelFormat;

/// The options fire builds every chain with: 8-bit colour averaged in linear light (fire uploads
/// it as `*_SRGB`), as many levels as the dimensions allow, threads from the budget.
pub fn options() -> fire_decode::mips::MipOptions {
    fire_decode::mips::MipOptions::new(true)
}

/// Bytes per texel of `format` as the decoder hands it over.
pub fn bytes_per_texel(format: PixelFormat) -> usize {
    match format {
        PixelFormat::Rgba8Unorm => 4,
        PixelFormat::Rgba16Unorm | PixelFormat::Rgba16Float => 8,
        PixelFormat::Rgba32Float => 16,
    }
}

// --- half floats ----------------------------------------------------------------------------

/// IEEE binary32 → binary16, round-to-nearest-even, overflow to infinity, NaN preserved.
pub fn f32_to_f16_bits(x: f32) -> u16 {
    let b = x.to_bits();
    let sign = ((b >> 16) & 0x8000) as u16;
    let exp = ((b >> 23) & 0xff) as i32;
    let mant = b & 0x7f_ffff;
    if exp == 0xff {
        // Inf / NaN (keep a payload bit so a NaN stays a NaN).
        return sign | 0x7c00 | if mant != 0 { 0x200 } else { 0 };
    }
    let e = exp - 127 + 15;
    if e >= 0x1f {
        return sign | 0x7c00; // overflow → inf
    }
    if e <= 0 {
        if e < -10 {
            return sign; // too small even for a subnormal → ±0
        }
        // Subnormal: shift the (implicit-1) mantissa down, rounding to nearest even.
        let m = mant | 0x80_0000;
        let shift = (14 - e) as u32;
        let mut half = (m >> shift) as u16;
        let rem = m & ((1u32 << shift) - 1);
        let halfway = 1u32 << (shift - 1);
        if rem > halfway || (rem == halfway && (half & 1) == 1) {
            half += 1;
        }
        return sign | half;
    }
    let mut half = ((e as u32) << 10) as u16 | (mant >> 13) as u16;
    let rem = mant & 0x1fff;
    // Rounding up may carry into the exponent; that is the correct result (it rounds to the next
    // binade, or to infinity at the top).
    if rem > 0x1000 || (rem == 0x1000 && (half & 1) == 1) {
        half = half.wrapping_add(1);
    }
    sign | half
}

/// Reinterpret 16-bit unorm samples as float16 of the same 0..1 value.
pub fn u16_to_f16(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    for px in bytes.as_chunks::<2>().0 {
        let v = u16::from_le_bytes(*px) as f32 / 65535.0;
        out.extend_from_slice(&f32_to_f16_bits(v).to_le_bytes());
    }
    out
}

/// A conversion of a level's bytes into the format it is uploaded as ([`u16_to_f16`],
/// [`f32_to_f16`]), for a device that lacks the source's own.
pub type Convert = fn(&[u8]) -> Vec<u8>;

/// Narrow float32 samples to float16.
pub fn f32_to_f16(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for px in bytes.as_chunks::<4>().0 {
        let v = f32::from_le_bytes(*px);
        out.extend_from_slice(&f32_to_f16_bits(v).to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f16_conversion_matches_the_known_encodings() {
        for (v, bits) in [
            (0.0f32, 0x0000u16),
            (-0.0, 0x8000),
            (1.0, 0x3c00),
            (-1.0, 0xbc00),
            (0.5, 0x3800),
            (2.0, 0x4000),
            (1024.0, 0x6400),
            (65504.0, 0x7bff),
            (6.103_515_6e-5, 0x0400), // the smallest normal
            (5.960_464_5e-8, 0x0001), // the smallest subnormal
            (3.051_757_8e-5, 0x0200), // a subnormal
            (1.0 / 3.0, 0x3555),
        ] {
            assert_eq!(f32_to_f16_bits(v), bits, "{v}");
        }
        assert_eq!(f32_to_f16_bits(1e6), 0x7c00); // overflow → +inf
        assert_eq!(f32_to_f16_bits(-1e6), 0xfc00);
        assert_eq!(f32_to_f16_bits(f32::NAN) & 0x7c00, 0x7c00);
        assert_ne!(f32_to_f16_bits(f32::NAN) & 0x3ff, 0, "a NaN stays a NaN");
        assert_eq!(f32_to_f16_bits(1e-9), 0); // underflow → 0
    }

    #[test]
    fn mip_chain_length() {
        assert_eq!(level_count(1, 1), 1);
        assert_eq!(level_count(2, 1), 2);
        assert_eq!(level_count(256, 128), 9);
        assert_eq!(level_count(1000, 3), 10);
    }

    /// Levels floor-halve and stop at 1, on each axis independently — the rule a DDS stores its
    /// own chain by, which is what lets one be adopted without rescaling.
    #[test]
    fn level_dims_floor_halve_and_never_reach_zero() {
        assert_eq!(level_dims(8, 4, 0), (8, 4));
        assert_eq!(level_dims(8, 4, 2), (2, 1));
        assert_eq!(level_dims(8, 4, 3), (1, 1));
        // Past the end of the chain, and past a shift that would be undefined.
        assert_eq!(level_dims(8, 4, 99), (1, 1));
        // Non-power-of-two floors rather than rounds.
        assert_eq!(level_dims(5, 3, 1), (2, 1));
    }
}
