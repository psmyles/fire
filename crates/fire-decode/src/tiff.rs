//! TIFF, decoded directly against the `tiff` crate.
//!
//! TIFF used to go through the `image` crate like TGA and ICO do, and that cost real pixels.
//! `image` can only represent what `tiff` hands it as a *named* colour type, and `tiff`'s
//! `colortype()` is deliberately conservative, so three things fell off the edge:
//!
//! - **A 4th sample the file did not label as alpha was dropped.** Photoshop writes its extra
//!   channel with `ExtraSamples = 0` (*unspecified*), which is not alpha by the letter of TIFF
//!   6.0, so `colortype()` reported plain `RGB` and readout discarded the sample. A texture
//!   sheet with white RGB and its whole shape in alpha decoded to a blank white square.
//! - **Grey + alpha would not open at all.** Two samples under `BlackIsZero` come back as
//!   `Multiband { num_samples: 2 }`, which `image` maps to `Unknown(16)` and refuses.
//! - **16-bit was silently narrowed to 8.** `image`'s adapter only special-cases float, so a
//!   16-bit scan lost half its depth while the status bar went on calling it 16-bit.
//!
//! What this module owns is only the *interpretation* of the samples. Every hard part —
//! LZW/Deflate/PackBits, strips vs. tiles, predictors, planar configuration, endianness — stays
//! inside the `tiff` crate, which is the same code `image` was driving. Colour types we have
//! nothing better to say about (palette, CMYK, YCbCr, Lab) fall back to the `image` path, which
//! already converts them correctly; [`decode`] returns `None` to ask for that.

use std::borrow::Cow;
use std::io::Cursor;

use tiff::decoder::{Decoder, DecodingResult, Limits};
use tiff::tags::Tag;
use tiff::ColorType;

use crate::{check_dims, DecodeError, DecodedImage, PixelFormat};

/// `ExtraSamples` value 1 — the alpha is *associated*, i.e. the colour samples are already
/// multiplied by it. Everything downstream (and the shader, which composites
/// `backdrop*(1-a) + rgb*a`) expects straight alpha, so this has to be undone on the way in.
const EXTRA_ASSOCIATED_ALPHA: u16 = 1;

/// Decode a TIFF, or return `None` if its colour type is one the caller should hand to the
/// `image` crate instead.
pub(crate) fn decode(bytes: &[u8]) -> Option<Result<DecodedImage, DecodeError>> {
    // Limits::unlimited defers the size question to our own guard below, which is expressed in
    // the same byte budget every other backend uses rather than this crate's private defaults.
    let mut dec = Decoder::new(Cursor::new(bytes))
        .ok()?
        .with_limits(Limits::unlimited());

    let color = dec.colortype().ok()?;
    let (width, height) = dec.dimensions().ok()?;

    // What we will emit, and how wide a sample is. `Multiband` is how the crate reports a
    // greyscale image that carries extra samples, so 1 and 2 bands are grey and grey+alpha.
    let (src_channels, bits) = match color {
        ColorType::Gray(b) => (1u8, b),
        ColorType::GrayA(b) => (2, b),
        ColorType::RGB(b) => (3, b),
        ColorType::RGBA(b) => (4, b),
        ColorType::Multiband {
            bit_depth,
            num_samples: 1,
        } => (1, bit_depth),
        ColorType::Multiband {
            bit_depth,
            num_samples: 2,
        } => (2, bit_depth),
        // Palette / CMYK / CMYKA / YCbCr / Lab, and multiband images with more bands than we
        // can assign meaning to: let the `image` crate's conversions handle them.
        _ => return None,
    };
    // Four output lanes at the source's own sample width. Sub-byte depths (1/2/4-bit bilevel
    // and packed palettes) are left to the `image` crate's unpacking.
    let out_bpp = match bits {
        8 | 16 | 32 => 4 * (bits as usize / 8),
        _ => return None,
    };
    if let Err(e) = check_dims(width as usize, height as usize, out_bpp, "TIFF") {
        return Some(Err(e));
    }

    // Associated alpha means premultiplied. Read it before the samples so the un-premultiply
    // below is decided by the file rather than guessed from the pixels.
    let premultiplied = src_channels % 2 == 0
        && dec
            .find_tag_unsigned_vec::<u16>(Tag::ExtraSamples)
            .ok()
            .flatten()
            .and_then(|v| v.first().copied())
            == Some(EXTRA_ASSOCIATED_ALPHA);
    let icc = dec.get_tag_u8_vec(Tag::IccProfile).ok();

    let samples = match dec.read_image() {
        Ok(s) => s,
        Err(e) => return Some(Err(DecodeError::Malformed(e.to_string()))),
    };

    let n = (width as usize).saturating_mul(height as usize);
    // `expand` indexes `channels` samples per pixel; a file whose readout came back short would
    // panic there. The decode path is a validation boundary, so refuse it instead.
    let want = n.saturating_mul(src_channels as usize);
    let got = match &samples {
        DecodingResult::U8(v) => v.len(),
        DecodingResult::U16(v) => v.len(),
        DecodingResult::F32(v) => v.len(),
        _ => 0,
    };
    if got < want {
        return Some(Err(DecodeError::Malformed(
            "TIFF sample data is shorter than its dimensions declare".into(),
        )));
    }
    let (pixels, format, bit_depth) = match samples {
        DecodingResult::U8(v) => (
            expand::<u8>(&v, n, src_channels, 255, premultiplied),
            PixelFormat::Rgba8Unorm,
            8u8,
        ),
        DecodingResult::U16(v) => {
            let rgba = expand::<u16>(&v, n, src_channels, u16::MAX, premultiplied);
            (to_ne_bytes_u16(&rgba), PixelFormat::Rgba16Unorm, 16)
        }
        DecodingResult::F32(v) => {
            let rgba = expand::<f32>(&v, n, src_channels, 1.0, premultiplied);
            (to_ne_bytes_f32(&rgba), PixelFormat::Rgba32Float, 32)
        }
        // 64-bit, signed, and half-float TIFFs are rare enough that the `image` crate's
        // conversions are a better answer than a hand-rolled one here.
        _ => return None,
    };

    Some(Ok(DecodedImage {
        pixels,
        width,
        height,
        format,
        bit_depth,
        channels: src_channels,
        icc,
        source_format: "TIFF",
        alpha_opaque: false, // set by `decode` after the final buffer is built
        downscaled_from: None,
        animation: None,
    }))
}

/// One sample type's worth of "widen to RGBA".
trait Sample: Copy {
    /// Straighten a premultiplied sample: `c / a`, saturating, with `a == 0` leaving it at 0.
    fn unpremultiply(c: Self, a: Self, opaque: Self) -> Self;
}

impl Sample for u8 {
    fn unpremultiply(c: u8, a: u8, _opaque: u8) -> u8 {
        if a == 0 {
            0
        } else {
            ((c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8
        }
    }
}

impl Sample for u16 {
    fn unpremultiply(c: u16, a: u16, _opaque: u16) -> u16 {
        if a == 0 {
            0
        } else {
            ((c as u64 * 65535 + a as u64 / 2) / a as u64).min(65535) as u16
        }
    }
}

impl Sample for f32 {
    fn unpremultiply(c: f32, a: f32, _opaque: f32) -> f32 {
        if a <= 0.0 {
            0.0
        } else {
            c / a
        }
    }
}

/// Widen `src` (1, 2, 3 or 4 samples per pixel) to interleaved RGBA, replicating grey across
/// the colour lanes and filling a missing alpha with `opaque`. Un-premultiplies when the file
/// declared associated alpha.
fn expand<T: Sample>(
    src: &[T],
    pixels: usize,
    channels: u8,
    opaque: T,
    premultiplied: bool,
) -> Vec<T> {
    let cpp = channels as usize;
    let mut out = Vec::with_capacity(pixels * 4);
    for i in 0..pixels {
        let s = &src[i * cpp..];
        let (r, g, b, a) = match channels {
            1 => (s[0], s[0], s[0], opaque),
            2 => (s[0], s[0], s[0], s[1]),
            3 => (s[0], s[1], s[2], opaque),
            _ => (s[0], s[1], s[2], s[3]),
        };
        if premultiplied {
            out.extend_from_slice(&[
                T::unpremultiply(r, a, opaque),
                T::unpremultiply(g, a, opaque),
                T::unpremultiply(b, a, opaque),
                a,
            ]);
        } else {
            out.extend_from_slice(&[r, g, b, a]);
        }
    }
    out
}

/// The CPU shader reads `Rgba16Unorm` / `Rgba32Float` back as native-endian, matching the
/// other backends.
fn to_ne_bytes_u16(v: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 2);
    for s in v {
        out.extend_from_slice(&s.to_ne_bytes());
    }
    out
}

fn to_ne_bytes_f32(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for s in v {
        out.extend_from_slice(&s.to_ne_bytes());
    }
    out
}

/// Rewrite a TIFF's lone *unspecified* `ExtraSamples` entry to *unassociated alpha*.
///
/// Photoshop stores the extra channel it names "Alpha 1" with `ExtraSamples = 0` (unspecified)
/// rather than 2 (unassociated alpha). By the letter of TIFF 6.0 that sample then carries no
/// defined meaning, and the `tiff` crate honors it literally: `colortype()` subtracts the extra
/// samples from the sample count, reports a 4-sample RGB image as plain `RGB`, and readout
/// discards the fourth sample. That has to be corrected *before* the decoder is built, because
/// by the time we can ask for pixels the decision is already made — hence a byte patch rather
/// than a branch.
///
/// Photoshop itself shows the channel, and every other viewer reads a single extra sample on an
/// RGB image as alpha, so we do too. Only IFD0 is walked, and only the single inline `SHORT`
/// form Photoshop writes is touched; an extra channel that means something else declares itself
/// with a different value and is left alone. When there is nothing to patch — the overwhelmingly
/// common case — the bytes are borrowed untouched and nothing is copied.
pub(crate) fn extra_sample_as_alpha(bytes: &[u8]) -> Cow<'_, [u8]> {
    const UNASSOCIATED_ALPHA: u16 = 2;

    let Some(at) = unspecified_extra_sample(bytes) else {
        return Cow::Borrowed(bytes);
    };
    let big_endian = bytes[0] == b'M';
    let mut out = bytes.to_vec();
    out[at..at + 2].copy_from_slice(&if big_endian {
        UNASSOCIATED_ALPHA.to_be_bytes()
    } else {
        UNASSOCIATED_ALPHA.to_le_bytes()
    });
    Cow::Owned(out)
}

/// Byte offset of IFD0's `ExtraSamples` value, if the file is a classic TIFF whose sole extra
/// sample is declared *unspecified*. Every read is bounds-checked, so a truncated or malformed
/// header — or a BigTIFF — yields `None` rather than reaching for a byte that isn't there.
fn unspecified_extra_sample(bytes: &[u8]) -> Option<usize> {
    const EXTRA_SAMPLES: u16 = 338;
    const SHORT: u16 = 3;

    let little_endian = match bytes.get(..4)? {
        [b'I', b'I', 42, 0] => true,
        [b'M', b'M', 0, 42] => false,
        _ => return None,
    };
    let u16at = |o: usize| -> Option<u16> {
        let b = bytes.get(o..o.checked_add(2)?)?.try_into().ok()?;
        Some(if little_endian {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        })
    };
    let u32at = |o: usize| -> Option<u32> {
        let b = bytes.get(o..o.checked_add(4)?)?.try_into().ok()?;
        Some(if little_endian {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    };

    let ifd = u32at(4)? as usize;
    for i in 0..u16at(ifd)? as usize {
        let entry = ifd.checked_add(2)?.checked_add(i.checked_mul(12)?)?;
        if u16at(entry)? != EXTRA_SAMPLES {
            continue;
        }
        // One SHORT fits the entry's 4-byte value field, so it is stored inline and
        // left-justified there (in both byte orders). More than one extra channel, or any
        // other type, is beyond what this fixup claims to understand.
        if u16at(entry + 2)? != SHORT || u32at(entry + 4)? != 1 {
            return None;
        }
        return (u16at(entry + 8)? == 0).then_some(entry + 8);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{decode, DecodeOptions};

    /// Build an uncompressed single-strip TIFF with full control over the sample layout.
    ///
    /// Hand-rolled on purpose: the `image` crate's TIFF encoder can only write the handful of
    /// colour types it models, and every case worth testing here is one it refuses to produce —
    /// an unlabelled extra sample, grey+alpha, associated alpha.
    fn build(
        w: u32,
        h: u32,
        photometric: u32,
        samples: u32,
        bits: u16,
        extra: Option<u32>,
        data: &[u8],
    ) -> Vec<u8> {
        let mut tags: Vec<(u16, u16, u32, u32)> = vec![
            (256, 3, 1, w),       // ImageWidth
            (257, 3, 1, h),       // ImageLength
            (258, 3, samples, 0), // BitsPerSample, value patched below
            (259, 3, 1, 1),       // Compression = none
            (262, 3, 1, photometric),
            (273, 4, 1, 0), // StripOffsets, patched below
            (277, 3, 1, samples),
            (278, 3, 1, h),
            (279, 4, 1, data.len() as u32),
        ];
        if let Some(e) = extra {
            tags.push((338, 3, 1, e)); // ExtraSamples
        }
        tags.sort_by_key(|t| t.0); // entries must ascend by tag

        let n = tags.len();
        let ifd_off = 8usize;
        let after_ifd = ifd_off + 2 + n * 12 + 4;
        // One or two SHORTs pack into the entry's own 4-byte value field; more need storage
        // after the IFD.
        let bits_inline = samples <= 2;
        let data_off = if bits_inline {
            after_ifd
        } else {
            after_ifd + samples as usize * 2
        };
        for t in &mut tags {
            match t.0 {
                258 if bits_inline => t.3 = (0..samples).map(|i| (bits as u32) << (16 * i)).sum(),
                258 => t.3 = after_ifd as u32,
                273 => t.3 = data_off as u32,
                _ => {}
            }
        }

        let mut b = Vec::new();
        b.extend_from_slice(b"II");
        b.extend_from_slice(&42u16.to_le_bytes());
        b.extend_from_slice(&(ifd_off as u32).to_le_bytes());
        b.extend_from_slice(&(n as u16).to_le_bytes());
        for (tag, typ, cnt, val) in &tags {
            b.extend_from_slice(&tag.to_le_bytes());
            b.extend_from_slice(&typ.to_le_bytes());
            b.extend_from_slice(&cnt.to_le_bytes());
            b.extend_from_slice(&val.to_le_bytes());
        }
        b.extend_from_slice(&0u32.to_le_bytes()); // no next IFD
        if !bits_inline {
            for _ in 0..samples {
                b.extend_from_slice(&bits.to_le_bytes());
            }
        }
        assert_eq!(b.len(), data_off);
        b.extend_from_slice(data);
        b
    }

    const RGB: u32 = 2;
    const BLACK_IS_ZERO: u32 = 1;

    fn run(bytes: &[u8]) -> crate::DecodedImage {
        decode(bytes, Some("tif"), &DecodeOptions::default()).expect("should decode")
    }

    /// Photoshop's "Alpha 1" — `ExtraSamples = 0`, *unspecified* — is read as alpha.
    ///
    /// `colortype()` subtracts extra samples from the sample count, so a 4-sample RGB image
    /// with an unspecified extra came back as plain `RGB` and readout dropped the fourth
    /// sample. A texture sheet with white RGB and its shape in alpha became a white square.
    #[test]
    fn unspecified_extra_sample_is_read_as_alpha() {
        let out = run(&build(1, 1, RGB, 4, 8, Some(0), &[255, 255, 255, 128]));
        assert_eq!(out.channels, 4);
        assert_eq!(out.pixels, vec![255, 255, 255, 128]);

        // A file that already says "unassociated alpha" needs no patch and decodes the same.
        let declared = build(1, 1, RGB, 4, 8, Some(2), &[255, 255, 255, 128]);
        assert!(matches!(extra_sample_as_alpha(&declared), Cow::Borrowed(_)));
        assert_eq!(run(&declared).pixels, vec![255, 255, 255, 128]);
    }

    /// Greyscale + alpha opens at all.
    ///
    /// Two samples under `BlackIsZero` are reported as `Multiband { num_samples: 2 }`, which the
    /// `image` crate maps to `Unknown(16)` and refuses outright — the file simply would not open.
    #[test]
    fn grayscale_plus_alpha_decodes() {
        for extra in [Some(2), Some(0), None] {
            let out = run(&build(
                2,
                1,
                BLACK_IS_ZERO,
                2,
                8,
                extra,
                &[200, 128, 0, 255],
            ));
            assert_eq!(out.channels, 2, "grey + alpha (ExtraSamples {extra:?})");
            assert_eq!(
                out.pixels,
                vec![200, 200, 200, 128, 0, 0, 0, 255],
                "grey replicates across RGB and the second sample is alpha"
            );
        }
    }

    /// 16-bit TIFFs keep 16 bits. The `image` adapter special-cased only float, so everything
    /// else went through `to_rgba8()` and lost half its depth while `bit_depth` still said 16.
    #[test]
    fn sixteen_bit_is_not_narrowed() {
        let le = |v: [u16; 4]| -> Vec<u8> { v.iter().flat_map(|s| s.to_le_bytes()).collect() };
        let out = run(&build(
            1,
            1,
            RGB,
            4,
            16,
            Some(2),
            &le([65535, 4660, 0, 32768]),
        ));
        assert_eq!(out.format, PixelFormat::Rgba16Unorm);
        assert_eq!(out.bit_depth, 16);
        let s: Vec<u16> = out
            .pixels
            .chunks_exact(2)
            .map(|c| u16::from_ne_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(
            s,
            vec![65535, 4660, 0, 32768],
            "exact 16-bit samples, not 8-bit rounded"
        );

        // Grey at 16 bits too, since that is the other path through `expand`.
        let out = run(&build(
            1,
            1,
            BLACK_IS_ZERO,
            1,
            16,
            None,
            &4660u16.to_le_bytes(),
        ));
        assert_eq!(out.format, PixelFormat::Rgba16Unorm);
        assert_eq!(out.channels, 1);
    }

    /// Associated (`ExtraSamples = 1`) alpha is premultiplied and must be straightened.
    ///
    /// The shader composites `backdrop*(1-a) + rgb*a`, i.e. it assumes straight alpha, so
    /// handing it premultiplied samples renders semi-transparent areas roughly twice too dark.
    #[test]
    fn associated_alpha_is_unpremultiplied() {
        // Full red at 50% alpha, stored premultiplied: 255*0.5 = 128.
        let out = run(&build(1, 1, RGB, 4, 8, Some(1), &[128, 0, 0, 128]));
        assert_eq!(
            out.pixels,
            vec![255, 0, 0, 128],
            "colour restored to its straight value"
        );

        // The same samples labelled unassociated are already straight and must not be touched.
        let out = run(&build(1, 1, RGB, 4, 8, Some(2), &[128, 0, 0, 128]));
        assert_eq!(out.pixels, vec![128, 0, 0, 128]);

        // Fully transparent premultiplied pixels carry no colour to recover; they must not
        // divide by zero.
        let out = run(&build(1, 1, RGB, 4, 8, Some(1), &[0, 0, 0, 0]));
        assert_eq!(out.pixels, vec![0, 0, 0, 0]);
    }

    /// Colour types this module does not own still decode, via the `image` fallback.
    #[test]
    fn unowned_color_types_fall_back_to_the_image_crate() {
        // CMYK (photometric 5): full cyan ink. `image` converts it; we must not swallow it.
        let out = run(&build(1, 1, 5, 4, 8, None, &[255, 0, 0, 0]));
        assert_eq!(out.source_format, "TIFF");
        assert_eq!(out.pixels[..3], [0, 255, 255], "cyan survives the fallback");

        // Plain RGB and plain grey stay on the native path and report honest channel counts.
        assert_eq!(
            run(&build(2, 1, RGB, 3, 8, None, &[255, 0, 0, 0, 255, 0])).channels,
            3
        );
        assert_eq!(
            run(&build(2, 1, BLACK_IS_ZERO, 1, 8, None, &[64, 192])).channels,
            1
        );
    }

    /// The byte patch rewrites two bytes and nothing else, and only in a classic TIFF.
    #[test]
    fn extra_sample_patch_is_minimal_and_safe() {
        let original = build(1, 1, RGB, 4, 8, Some(0), &[1, 2, 3, 4]);
        let Cow::Owned(patched) = extra_sample_as_alpha(&original) else {
            panic!("expected a patched copy")
        };
        let diffs = patched
            .iter()
            .zip(&original)
            .filter(|(a, b)| a != b)
            .count();
        assert_eq!(diffs, 1, "exactly one byte differs");

        for bytes in [
            &build(1, 1, RGB, 3, 8, None, &[1, 2, 3])[..], // no ExtraSamples tag
            &build(1, 1, RGB, 4, 8, Some(2), &[1, 2, 3, 4])[..], // already alpha
            &original[..20],                               // truncated mid-IFD
            b"II\x2a\x00",                                 // header only
            b"\x89PNG\r\n\x1a\n",                          // not a TIFF
            b"",
        ] {
            assert!(matches!(extra_sample_as_alpha(bytes), Cow::Borrowed(_)));
        }
    }
}
