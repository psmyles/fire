//! The image's mip chain, built on the CPU.
//!
//! sokol_gfx has no `GenerateMips` and its rules rule out building the pyramid in place on the
//! GPU: an image that is rendered into cannot be created with data, and an image cannot be bound
//! as a texture in the pass that renders into it. So the chain is what sokol's documentation says
//! it should be — supplied by the application, every level as data, on the immutable image. It is
//! built here, on the decode worker, right after the decode and before the image is posted, so the
//! UI thread never pays for it and the upload is one `sg_make_image`.
//!
//! Each level is a 2×2 box filter of the level above (what a hardware `GenerateMips` does), with
//! the 8-bit sRGB source averaged in *linear* light — also what the hardware does for an `*_SRGB`
//! format — through two lookup tables. Odd edges clamp. Rows are split across threads for the
//! big levels; the small ones are not worth the thread hand-off.

use std::sync::OnceLock;

// The halving rule lives with the decode contract that states it (`DecodedImage::source_mips`).
pub use fire_decode::level_dims;
use fire_decode::PixelFormat;

/// The number of mip levels in a full chain for a `w`×`h` texture.
pub fn level_count(w: u32, h: u32) -> u32 {
    32 - w.max(h).max(1).leading_zeros()
}

/// Turn a file's own mip chain into the full one the uploader requires.
///
/// A decoder may hand over a chain that is short (a `.dds` is free to stop at 4x4), longer than
/// the dimensions allow, or simply wrong. Each supplied level is kept only while its length
/// matches what its dimensions demand; the first that does not ends the adopted run, because
/// past that point the file's own offsets are no longer trustworthy. The tail is then box-filtered
/// down from the last level kept — not from level 0 — so the authored levels stay upstream of
/// everything computed.
///
/// The alternative, throwing the whole chain away and rebuilding, would be simpler and worse:
/// authored mips are the reason to look at a texture's mips at all.
pub fn complete(
    supplied: Vec<Vec<u8>>,
    level0: &[u8],
    w: u32,
    h: u32,
    format: PixelFormat,
) -> Vec<Vec<u8>> {
    let bpp = bytes_per_texel(format);
    let levels = level_count(w, h) as usize;
    let mut out: Vec<Vec<u8>> = Vec::with_capacity(levels.saturating_sub(1));
    for level in supplied.into_iter().take(levels.saturating_sub(1)) {
        let (lw, lh) = level_dims(w, h, out.len() as u32 + 1);
        if level.len() != lw as usize * lh as usize * bpp {
            break;
        }
        out.push(level);
    }
    // Whatever the file did not carry, and the whole chain when it carried nothing usable.
    while out.len() + 1 < levels {
        let n = out.len() as u32;
        let (sw, sh) = level_dims(w, h, n);
        let (dw, dh) = level_dims(w, h, n + 1);
        let src: &[u8] = out.last().map_or(level0, |v| v.as_slice());
        if src.len() < sw as usize * sh as usize * bpp {
            return Vec::new();
        }
        out.push(downsample_level(src, sw, sh, dw, dh, format));
    }
    out
}

/// Bytes per texel of `format` as the decoder hands it over.
pub fn bytes_per_texel(format: PixelFormat) -> usize {
    match format {
        PixelFormat::Rgba8Unorm => 4,
        PixelFormat::Rgba16Unorm | PixelFormat::Rgba16Float => 8,
        PixelFormat::Rgba32Float => 16,
    }
}

/// Build levels `1..` of the chain for `pixels` (level 0, `w`×`h`, `format`). Each returned
/// buffer is one level, in `format`, smallest last; empty for a 1×1 image. A `pixels` shorter
/// than its dimensions declare yields an empty chain rather than a read past the end.
pub fn build(pixels: &[u8], w: u32, h: u32, format: PixelFormat) -> Vec<Vec<u8>> {
    let bpp = bytes_per_texel(format);
    let needed = (w as usize)
        .checked_mul(h as usize)
        .and_then(|n| n.checked_mul(bpp));
    if needed.is_none_or(|n| pixels.len() < n) {
        return Vec::new();
    }
    let levels = level_count(w, h);
    let mut out: Vec<Vec<u8>> = Vec::with_capacity(levels.saturating_sub(1) as usize);
    let (mut sw, mut sh) = (w, h);
    for _ in 1..levels {
        let (dw, dh) = ((sw / 2).max(1), (sh / 2).max(1));
        let next = {
            let src: &[u8] = out.last().map_or(pixels, |v| v.as_slice());
            downsample_level(src, sw, sh, dw, dh, format)
        };
        out.push(next);
        sw = dw;
        sh = dh;
    }
    out
}

/// One level's reduction, dispatched on the pixel format. Shared by [`build`] and [`complete`].
fn downsample_level(
    src: &[u8],
    sw: u32,
    sh: u32,
    dw: u32,
    dh: u32,
    format: PixelFormat,
) -> Vec<u8> {
    match format {
        PixelFormat::Rgba8Unorm => downsample::<4>(src, sw, sh, dw, dh, load_srgb8, store_srgb8),
        PixelFormat::Rgba16Unorm => downsample::<8>(src, sw, sh, dw, dh, load_u16, store_u16),
        PixelFormat::Rgba16Float => downsample::<8>(src, sw, sh, dw, dh, load_f16, store_f16),
        PixelFormat::Rgba32Float => downsample::<16>(src, sw, sh, dw, dh, load_f32, store_f32),
    }
}

/// Levels with at least this many texels are reduced on several threads.
const PARALLEL_MIN_TEXELS: usize = 256 * 256;

/// One 2×2 box-filter reduction of a `sw`×`sh` level (`E` bytes per texel) into `dw`×`dh`.
fn downsample<const E: usize>(
    src: &[u8],
    sw: u32,
    sh: u32,
    dw: u32,
    dh: u32,
    load: fn(&[u8]) -> [f32; 4],
    store: fn([f32; 4], &mut [u8]),
) -> Vec<u8> {
    let (sw, sh, dw, dh) = (sw as usize, sh as usize, dw as usize, dh as usize);
    let src_row = sw * E;
    let dst_row = dw * E;
    let mut out = vec![0u8; dst_row * dh];

    let reduce_rows = |first_row: usize, rows: &mut [u8]| {
        for (i, drow) in rows.chunks_exact_mut(dst_row).enumerate() {
            let y = first_row + i;
            let y0 = (2 * y).min(sh - 1);
            let y1 = (2 * y + 1).min(sh - 1);
            let r0 = &src[y0 * src_row..y0 * src_row + src_row];
            let r1 = &src[y1 * src_row..y1 * src_row + src_row];
            for (x, dst) in drow.as_chunks_mut::<E>().0.iter_mut().enumerate() {
                let x0 = (2 * x).min(sw - 1) * E;
                let x1 = (2 * x + 1).min(sw - 1) * E;
                let a = load(&r0[x0..x0 + E]);
                let b = load(&r0[x1..x1 + E]);
                let c = load(&r1[x0..x0 + E]);
                let d = load(&r1[x1..x1 + E]);
                let mut s = [0f32; 4];
                for ch in 0..4 {
                    s[ch] = (a[ch] + b[ch] + c[ch] + d[ch]) * 0.25;
                }
                store(s, dst);
            }
        }
    };

    let threads = if dw * dh >= PARALLEL_MIN_TEXELS {
        std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .clamp(1, 8)
            .min(dh)
    } else {
        1
    };
    if threads <= 1 {
        reduce_rows(0, &mut out);
    } else {
        let rows_per = dh.div_ceil(threads);
        std::thread::scope(|scope| {
            for (i, chunk) in out.chunks_mut(rows_per * dst_row).enumerate() {
                let reduce_rows = &reduce_rows;
                scope.spawn(move || reduce_rows(i * rows_per, chunk));
            }
        });
    }
    out
}

// --- texel codecs ---------------------------------------------------------------------------

/// sRGB byte → linear, for the 8-bit source's color channels.
fn srgb_to_linear_table() -> &'static [f32; 256] {
    static TABLE: OnceLock<[f32; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        std::array::from_fn(|i| {
            let c = i as f32 / 255.0;
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        })
    })
}

/// Linear (quantized to 1/4095) → sRGB byte. 4096 steps resolve the curve's steep dark end to
/// well under one output code, so the rounding error is that of the 8-bit result itself.
fn linear_to_srgb_table() -> &'static [u8; 4096] {
    static TABLE: OnceLock<[u8; 4096]> = OnceLock::new();
    TABLE.get_or_init(|| {
        std::array::from_fn(|i| {
            let c = i as f32 / 4095.0;
            let s = if c <= 0.003_130_8 {
                c * 12.92
            } else {
                1.055 * c.powf(1.0 / 2.4) - 0.055
            };
            (s * 255.0 + 0.5) as u8
        })
    })
}

fn load_srgb8(b: &[u8]) -> [f32; 4] {
    let t = srgb_to_linear_table();
    [
        t[b[0] as usize],
        t[b[1] as usize],
        t[b[2] as usize],
        b[3] as f32 / 255.0,
    ]
}

fn store_srgb8(v: [f32; 4], out: &mut [u8]) {
    let t = linear_to_srgb_table();
    let q = |x: f32| t[(x.clamp(0.0, 1.0) * 4095.0 + 0.5) as usize];
    out[0] = q(v[0]);
    out[1] = q(v[1]);
    out[2] = q(v[2]);
    out[3] = (v[3].clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
}

fn load_u16(b: &[u8]) -> [f32; 4] {
    std::array::from_fn(|i| u16::from_le_bytes([b[2 * i], b[2 * i + 1]]) as f32 / 65535.0)
}

fn store_u16(v: [f32; 4], out: &mut [u8]) {
    for (i, x) in v.iter().enumerate() {
        let q = (x.clamp(0.0, 1.0) * 65535.0 + 0.5) as u16;
        out[2 * i..2 * i + 2].copy_from_slice(&q.to_le_bytes());
    }
}

fn load_f16(b: &[u8]) -> [f32; 4] {
    std::array::from_fn(|i| f16_bits_to_f32(u16::from_le_bytes([b[2 * i], b[2 * i + 1]])))
}

fn store_f16(v: [f32; 4], out: &mut [u8]) {
    for (i, x) in v.iter().enumerate() {
        out[2 * i..2 * i + 2].copy_from_slice(&f32_to_f16_bits(*x).to_le_bytes());
    }
}

fn load_f32(b: &[u8]) -> [f32; 4] {
    std::array::from_fn(|i| {
        f32::from_le_bytes([b[4 * i], b[4 * i + 1], b[4 * i + 2], b[4 * i + 3]])
    })
}

fn store_f32(v: [f32; 4], out: &mut [u8]) {
    for (i, x) in v.iter().enumerate() {
        out[4 * i..4 * i + 4].copy_from_slice(&x.to_le_bytes());
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

/// IEEE binary16 → binary32 (exact).
pub fn f16_bits_to_f32(h: u16) -> f32 {
    let sign = ((h >> 15) & 1) as u32;
    let exp = ((h >> 10) & 0x1f) as i32;
    let mant = (h & 0x3ff) as u32;
    let bits = if exp == 0 {
        if mant == 0 {
            sign << 31
        } else {
            // Subnormal: normalize.
            let mut e = -1;
            let mut m = mant;
            while m & 0x400 == 0 {
                m <<= 1;
                e -= 1;
            }
            let e = (e + 1 + 127 - 15) as u32;
            (sign << 31) | (e << 23) | ((m & 0x3ff) << 13)
        }
    } else if exp == 0x1f {
        (sign << 31) | (0xff << 23) | (mant << 13)
    } else {
        (sign << 31) | (((exp - 15 + 127) as u32) << 23) | (mant << 13)
    };
    f32::from_bits(bits)
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
    fn f16_conversion_round_trips_representable_values() {
        for v in [
            0.0f32,
            1.0,
            -1.0,
            0.5,
            0.25,
            2.0,
            1024.0,
            65504.0,
            6.1035156e-5,
            1.0 / 3.0,
        ] {
            let h = f32_to_f16_bits(v);
            let back = f16_bits_to_f32(h);
            assert!(
                (back - v).abs() <= v.abs() * 1e-3 + 1e-7,
                "{v} -> {h:#06x} -> {back}"
            );
        }
        assert_eq!(f32_to_f16_bits(1e6), 0x7c00); // overflow → +inf
        assert_eq!(f32_to_f16_bits(-1e6), 0xfc00);
        assert!(f16_bits_to_f32(f32_to_f16_bits(f32::NAN)).is_nan());
        assert_eq!(f32_to_f16_bits(1e-9), 0); // underflow → 0
    }

    #[test]
    fn mip_chain_length() {
        assert_eq!(level_count(1, 1), 1);
        assert_eq!(level_count(2, 1), 2);
        assert_eq!(level_count(256, 128), 9);
        assert_eq!(level_count(1000, 3), 10);
    }

    #[test]
    fn chain_has_one_buffer_per_level_with_halved_dims() {
        let (w, h) = (5u32, 3u32);
        let px = vec![0u8; (w * h * 4) as usize];
        let chain = build(&px, w, h, PixelFormat::Rgba8Unorm);
        assert_eq!(chain.len(), 2); // 5×3 → 2×1 → 1×1
        assert_eq!(chain[0].len(), 2 * 4); // 2×1 px, 4 bytes each
        assert_eq!(chain[1].len(), 4);
        assert!(build(&px[..8], w, h, PixelFormat::Rgba8Unorm).is_empty());
    }

    #[test]
    fn box_filter_averages_in_linear_light() {
        // Black and white average to linear 0.5, which is sRGB 188 — not 128.
        let px = [
            0, 0, 0, 255, 255, 255, 255, 255, 0, 0, 0, 255, 255, 255, 255, 255,
        ];
        let chain = build(&px, 2, 2, PixelFormat::Rgba8Unorm);
        assert_eq!(chain.len(), 1);
        assert_eq!(&chain[0][..3], &[188, 188, 188]);
        assert_eq!(chain[0][3], 255);
    }

    #[test]
    fn float_levels_average_exactly() {
        let mut px = Vec::new();
        for v in [1.0f32, 3.0, 5.0, 7.0] {
            for _ in 0..4 {
                px.extend_from_slice(&v.to_le_bytes());
            }
        }
        let chain = build(&px, 2, 2, PixelFormat::Rgba32Float);
        assert_eq!(load_f32(&chain[0][..16]), [4.0; 4]);
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

    /// A solid buffer of `v` at `w`x`h`, RGBA8.
    fn solid(w: u32, h: u32, v: u8) -> Vec<u8> {
        vec![v; w as usize * h as usize * 4]
    }

    /// A full chain from the file is taken exactly as given: the point of an authored chain is
    /// that its levels are *not* what a box filter would have produced.
    #[test]
    fn complete_adopts_a_full_chain_untouched() {
        let level0 = solid(4, 4, 10);
        let supplied = vec![solid(2, 2, 200), solid(1, 1, 250)];
        let chain = complete(supplied.clone(), &level0, 4, 4, PixelFormat::Rgba8Unorm);
        assert_eq!(chain, supplied, "no level was recomputed");
    }

    /// A file may stop its chain early. What it supplied is kept, and only the tail is computed —
    /// from the last authored level, not from level 0, so the authored data stays upstream.
    #[test]
    fn complete_pads_a_partial_chain_from_the_last_authored_level() {
        // Level 0 is black, but the file's level 1 is white. A correctly padded level 2 must come
        // from the white level 1, not from the black level 0.
        let level0 = solid(4, 4, 0);
        let chain = complete(
            vec![solid(2, 2, 255)],
            &level0,
            4,
            4,
            PixelFormat::Rgba8Unorm,
        );
        assert_eq!(chain.len(), 2, "4x4 has three levels in all");
        assert_eq!(chain[0], solid(2, 2, 255));
        assert_eq!(
            chain[1],
            solid(1, 1, 255),
            "padded from level 1, not level 0"
        );
    }

    /// A level whose length does not match its dimensions ends the adopted run: past a bad
    /// offset the file's own layout is no longer trustworthy, so the rest is recomputed.
    #[test]
    fn complete_stops_adopting_at_a_wrong_sized_level() {
        let level0 = solid(4, 4, 0);
        let chain = complete(
            vec![solid(2, 2, 255), vec![1u8; 3]],
            &level0,
            4,
            4,
            PixelFormat::Rgba8Unorm,
        );
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0], solid(2, 2, 255));
        assert_eq!(chain[1], solid(1, 1, 255), "the short level was recomputed");
    }

    /// A header can claim more levels than the dimensions allow. The extras are dropped rather
    /// than uploaded — sokol takes at most 16, and a chain longer than `level_count` would index
    /// past its mip-level array.
    #[test]
    fn complete_drops_levels_past_the_end_of_the_chain() {
        let level0 = solid(2, 2, 0);
        let chain = complete(
            vec![solid(1, 1, 9), solid(1, 1, 9), solid(1, 1, 9)],
            &level0,
            2,
            2,
            PixelFormat::Rgba8Unorm,
        );
        assert_eq!(chain.len(), level_count(2, 2) as usize - 1);
    }

    /// No usable chain at all falls back to building the whole thing, which is the pre-existing
    /// behaviour for every format but DDS.
    #[test]
    fn complete_with_nothing_supplied_matches_build() {
        let level0 = solid(4, 4, 40);
        assert_eq!(
            complete(Vec::new(), &level0, 4, 4, PixelFormat::Rgba8Unorm),
            build(&level0, 4, 4, PixelFormat::Rgba8Unorm)
        );
    }

    /// A level 0 shorter than its dimensions declare yields no chain rather than a read past the
    /// end — the same refusal `build` makes.
    #[test]
    fn complete_refuses_a_short_level_zero() {
        assert!(complete(Vec::new(), &[0u8; 3], 4, 4, PixelFormat::Rgba8Unorm).is_empty());
    }
}
