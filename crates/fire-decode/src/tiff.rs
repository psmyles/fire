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
//! inside the `tiff` crate, which is the same code `image` was driving. Palette images, which
//! neither crate will read, are relabelled and mapped here ([`decode_palette`]). Colour types we
//! have nothing better to say about (CMYK, YCbCr, Lab) fall back to the `image` path, which
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
    if let Some(palette) = decode_palette(bytes) {
        return Some(palette);
    }
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
        // CMYK / CMYKA / YCbCr / Lab, and multiband images with more bands than we can assign
        // meaning to: let the `image` crate's conversions handle them.
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

    // The readout below hands back `src_channels` samples per pixel — the crate trims any
    // further extra channels (spot/unspecified samples) itself — but its *file-side* buffers
    // are laid out from the full SamplesPerPixel, which `colortype()` hides. The check above
    // bounds only the output; with `Limits::unlimited()` deferring the size question to us, a
    // crafted SamplesPerPixel would otherwise inflate the intermediate strip buffers past the
    // byte budget unchecked, and an allocation that fails aborts rather than erroring.
    let file_spp = dec
        .find_tag_unsigned::<u16>(Tag::SamplesPerPixel)
        .ok()
        .flatten()
        .map_or(src_channels as usize, usize::from);
    if file_spp < src_channels as usize {
        // Fewer samples than the named colour type needs: malformed; let the image crate say so.
        return None;
    }
    if let Err(e) = check_dims(
        width as usize,
        height as usize,
        file_spp * (bits as usize / 8),
        "TIFF",
    ) {
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
    // `.filter`: a present-but-empty profile tag must surface as "no profile", the way the PNG
    // and image-crate paths already read it, not as `Some(vec![])` for consumers to trip on.
    let icc = dec
        .get_tag_u8_vec(Tag::IccProfile)
        .ok()
        .filter(|v| !v.is_empty());

    let samples = match read_strips_parallel(bytes, &mut dec).unwrap_or_else(|| dec.read_image()) {
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
    // The CPU shader reads Rgba16Unorm / Rgba32Float back as native-endian, matching the other
    // backends.
    let (pixels, format, bit_depth) = match samples {
        // Straight RGBA8 is already the output layout: hand the buffer over rather than copy it.
        DecodingResult::U8(mut v) if src_channels == 4 && !premultiplied => {
            v.truncate(want);
            (v, PixelFormat::Rgba8Unorm, 8u8)
        }
        DecodingResult::U8(v) => (
            widen(&v, n, src_channels, 255, premultiplied),
            PixelFormat::Rgba8Unorm,
            8u8,
        ),
        DecodingResult::U16(v) => (
            widen(&v, n, src_channels, u16::MAX, premultiplied),
            PixelFormat::Rgba16Unorm,
            16,
        ),
        DecodingResult::F32(v) => (
            widen(&v, n, src_channels, 1.0, premultiplied),
            PixelFormat::Rgba32Float,
            32,
        ),
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
        source_mips: None,
        layout: None,
        animation: None,
    }))
}

/// Read the image's samples one strip per task, across threads, into the same interleaved buffer
/// `read_image` would return. `None` when the layout is not one this handles — tiles, planar
/// configuration, a single strip, a sample type other than 8/16-bit unsigned or 32-bit float —
/// and the caller reads the image the ordinary, sequential way.
///
/// Strips are independent by construction (each is compressed on its own, so a decoder can start
/// at any of them), and a compressed TIFF is all strip decoding: an 8192² LZW or Deflate file took
/// ~1.2 s / ~0.9 s on one thread against ~0.15 s uncompressed. Each thread opens its own decoder
/// over the same bytes — re-reading the IFD costs microseconds — and decodes a contiguous run of
/// strips straight into its own slice of the output, which is exactly the row range they cover.
fn read_strips_parallel(
    bytes: &[u8],
    dec: &mut Decoder<Cursor<&[u8]>>,
) -> Option<tiff::TiffResult<DecodingResult>> {
    use tiff::decoder::{ChunkType, DecodingSampleType};

    if dec.get_chunk_type() != ChunkType::Strip
        || dec
            .find_tag_unsigned::<u16>(Tag::PlanarConfiguration)
            .ok()
            .flatten()
            .is_some_and(|p| p != 1)
    {
        return None;
    }
    let strips = dec.strip_count().ok()? as usize;
    let layout = dec.image_buffer_layout().ok()?;
    let rows_per_strip = dec.chunk_dimensions().1 as usize;
    let row_stride = layout.row_stride?.get();
    let strip_bytes = rows_per_strip.checked_mul(row_stride)?;
    // Every strip but the last is exactly `strip_bytes`; the last is whatever is left. Anything
    // else (a strip layout that does not tile the image the way the arithmetic says) is left to
    // the crate.
    let last = strips.checked_sub(1)?;
    if strips < 2
        || dec.image_chunk_buffer_layout(0).ok()?.len != strip_bytes
        || last * strip_bytes + dec.image_chunk_buffer_layout(last as u32).ok()?.len != layout.len
    {
        return None;
    }

    fn read<T: bytemuck::Pod + Default + Send>(
        bytes: &[u8],
        total: usize,
        strip_bytes: usize,
    ) -> tiff::TiffResult<Vec<T>> {
        let mut out = vec![T::default(); total / std::mem::size_of::<T>()];
        let error = std::sync::Mutex::new(None);
        crate::par_chunks_mut(
            bytemuck::cast_slice_mut(&mut out),
            strip_bytes,
            |offset, part| {
                let result = (|| {
                    let mut dec =
                        Decoder::new(Cursor::new(bytes))?.with_limits(Limits::unlimited());
                    let first = offset / strip_bytes;
                    for (i, strip) in part.chunks_mut(strip_bytes).enumerate() {
                        dec.read_chunk_bytes((first + i) as u32, strip)?;
                    }
                    Ok(())
                })();
                if let Err(e) = result {
                    *error.lock().unwrap_or_else(|p| p.into_inner()) = Some(e);
                }
            },
        );
        match error.into_inner().unwrap_or_else(|p| p.into_inner()) {
            Some(e) => Err(e),
            None => Ok(out),
        }
    }

    let total = layout.len;
    Some(match layout.sample_type? {
        DecodingSampleType::U8 => read::<u8>(bytes, total, strip_bytes).map(DecodingResult::U8),
        DecodingSampleType::U16 => read::<u16>(bytes, total, strip_bytes).map(DecodingResult::U16),
        DecodingSampleType::F32 => read::<f32>(bytes, total, strip_bytes).map(DecodingResult::F32),
        _ => return None,
    })
}

/// Widen `pixels` pixels of `src` to RGBA bytes: [`crate::rgba_bytes`]' parallel pass for straight
/// alpha, the un-premultiplying [`expand`] for associated alpha.
fn widen<T: Sample + bytemuck::Pod + Sync>(
    src: &[T],
    pixels: usize,
    channels: u8,
    opaque: T,
    premultiplied: bool,
) -> Vec<u8> {
    if premultiplied {
        crate::to_ne_bytes(&expand(src, pixels, channels, opaque, true))
    } else {
        crate::rgba_bytes(
            &src[..pixels * channels as usize],
            channels as usize,
            opaque,
        )
    }
}

/// One sample type's worth of "widen to RGBA".
trait Sample: Copy {
    /// Straighten a premultiplied sample: `c / a`, saturating, with `a == 0` leaving it at 0.
    fn unpremultiply(c: Self, a: Self) -> Self;
}

impl Sample for u8 {
    fn unpremultiply(c: u8, a: u8) -> u8 {
        if a == 0 {
            0
        } else {
            ((c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8
        }
    }
}

impl Sample for u16 {
    fn unpremultiply(c: u16, a: u16) -> u16 {
        if a == 0 {
            0
        } else {
            ((c as u64 * 65535 + a as u64 / 2) / a as u64).min(65535) as u16
        }
    }
}

impl Sample for f32 {
    fn unpremultiply(c: f32, a: f32) -> f32 {
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
                T::unpremultiply(r, a),
                T::unpremultiply(g, a),
                T::unpremultiply(b, a),
                a,
            ]);
        } else {
            out.extend_from_slice(&[r, g, b, a]);
        }
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
    // More than one extra channel is beyond what this fixup claims to understand, and
    // `ifd0_short` only answers for a lone SHORT.
    ifd0_short(bytes, EXTRA_SAMPLES).and_then(|(at, value)| (value == 0).then_some(at))
}

/// IFD0's entry for `tag`, when it holds a single SHORT: the byte offset of that value (stored
/// inline, left-justified in the entry's 4-byte value field, in both byte orders) and the value.
/// `None` for a classic TIFF without the entry, an entry of any other type or count, a BigTIFF,
/// or a header too short or malformed to walk — every read is bounds-checked.
fn ifd0_short(bytes: &[u8], tag: u16) -> Option<(usize, u16)> {
    const SHORT: u16 = 3;

    let little_endian = match bytes.get(..4)? {
        [b'I', b'I', 42, 0] => true,
        [b'M', b'M', 0, 42] => false,
        _ => return None,
    };
    // The bounds-checked primitive readers are `exif`'s (which `raw` also imports) — the
    // third file to walk a TIFF IFD must not grow a third copy of them.
    let u16at = |o: usize| crate::exif::rd_u16(bytes, o, little_endian);
    let u32at = |o: usize| crate::exif::rd_u32(bytes, o, little_endian);

    let ifd = u32at(4)? as usize;
    for i in 0..u16at(ifd)? as usize {
        let entry = ifd.checked_add(2)?.checked_add(i.checked_mul(12)?)?;
        if u16at(entry)? != tag {
            continue;
        }
        if u16at(entry + 2)? != SHORT || u32at(entry + 4)? != 1 {
            return None;
        }
        return Some((entry + 8, u16at(entry + 8)?));
    }
    None
}

/// A palette TIFF (`PhotometricInterpretation` 3, "RGBPalette"): one index per pixel, looked up
/// in the `ColorMap` tag. `None` if the file is not one.
///
/// The `tiff` crate (0.11) refuses palette images outright — `colortype()` errors, and every
/// readout goes through it — and the `image` crate, which drives the same crate, refuses them
/// too, so a palette TIFF did not open at all. The indices themselves are ordinary 1/2/4/8/16-bit
/// greyscale samples, though, so the file is relabelled greyscale (`BlackIsZero`) in a copy of
/// its bytes — the same kind of one-field patch [`extra_sample_as_alpha`] makes — and read by the
/// crate as such, with all of its decompression, predictor and strip/tile handling intact. The
/// `ColorMap` tag is untouched by the relabelling and maps the indices to colour here.
pub(crate) fn decode_palette(bytes: &[u8]) -> Option<Result<DecodedImage, DecodeError>> {
    const PHOTOMETRIC_INTERPRETATION: u16 = 262;
    const RGB_PALETTE: u16 = 3;
    const BLACK_IS_ZERO: u16 = 1;

    let (at, value) = ifd0_short(bytes, PHOTOMETRIC_INTERPRETATION)?;
    if value != RGB_PALETTE {
        return None;
    }
    let mut patched = bytes.to_vec();
    let relabel = if bytes[0] == b'M' {
        BLACK_IS_ZERO.to_be_bytes()
    } else {
        BLACK_IS_ZERO.to_le_bytes()
    };
    patched[at..at + 2].copy_from_slice(&relabel);
    Some(read_palette(&patched))
}

fn read_palette(bytes: &[u8]) -> Result<DecodedImage, DecodeError> {
    let malformed = |what: &str| DecodeError::Malformed(format!("palette TIFF: {what}"));
    let err = |e: tiff::TiffError| DecodeError::Malformed(e.to_string());

    let mut dec = Decoder::new(Cursor::new(bytes))
        .map_err(err)?
        .with_limits(Limits::unlimited());
    let (width, height) = dec.dimensions().map_err(err)?;
    let bits = match dec.colortype().map_err(err)? {
        ColorType::Gray(b @ (1 | 2 | 4 | 8 | 16)) => b,
        _ => {
            return Err(malformed(
                "indices must be one 1/2/4/8/16-bit sample a pixel",
            ))
        }
    };
    check_dims(width as usize, height as usize, 4, "TIFF")?;
    // The file-side buffers are laid out from SamplesPerPixel (see `decode`); a palette image
    // has one, and one is all the guard above accounts for.
    if dec
        .find_tag_unsigned::<u16>(Tag::SamplesPerPixel)
        .ok()
        .flatten()
        .is_some_and(|spp| spp != 1)
    {
        return Err(malformed("more than one sample per pixel"));
    }

    // The map is all reds, then all greens, then all blues, 2^bits each, 16 bits a value. Very
    // old writers stored 8-bit values; like libtiff, read a map with nothing above 255 as that.
    let map = dec.get_tag_u16_vec(Tag::ColorMap).map_err(err)?;
    let entries = 1usize << bits;
    if map.len() < 3 * entries {
        return Err(malformed("ColorMap is shorter than the bit depth requires"));
    }
    let eight_bit_map = map.iter().all(|&v| v <= 255);
    let to8 = |v: u16| {
        if eight_bit_map {
            v as u8
        } else {
            ((u32::from(v) * 255 + 32767) / 65535) as u8
        }
    };
    let colors: Vec<[u8; 4]> = (0..entries)
        .map(|i| {
            [
                to8(map[i]),
                to8(map[entries + i]),
                to8(map[2 * entries + i]),
                255,
            ]
        })
        .collect();
    let icc = dec
        .get_tag_u8_vec(Tag::IccProfile)
        .ok()
        .filter(|v| !v.is_empty());

    let samples = read_strips_parallel(bytes, &mut dec)
        .unwrap_or_else(|| dec.read_image())
        .map_err(err)?;
    let (w, h) = (width as usize, height as usize);
    let mut pixels = vec![0u8; w * h * 4];
    match samples {
        // Sub-byte indices are packed most-significant first, each row starting on a byte.
        DecodingResult::U8(packed) => {
            let bits = bits as usize;
            let row_bytes = (w * bits).div_ceil(8);
            if packed.len() < row_bytes * h {
                return Err(malformed(
                    "index data is shorter than its dimensions declare",
                ));
            }
            let mask = (1u16 << bits) - 1;
            crate::par_chunks_mut(&mut pixels, w * 4, |offset, part| {
                let first = offset / (w * 4);
                for (y, row) in part.chunks_exact_mut(w * 4).enumerate() {
                    let src = &packed[(first + y) * row_bytes..][..row_bytes];
                    for (x, px) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                        let bit = x * bits;
                        let index = (u16::from(src[bit / 8]) >> (8 - bits - bit % 8)) & mask;
                        *px = colors[index as usize];
                    }
                }
            });
        }
        DecodingResult::U16(indices) => {
            if indices.len() < w * h {
                return Err(malformed(
                    "index data is shorter than its dimensions declare",
                ));
            }
            crate::par_chunks_mut(&mut pixels, 4, |offset, part| {
                let src = &indices[offset / 4..];
                for (px, &i) in part.as_chunks_mut::<4>().0.iter_mut().zip(src) {
                    *px = colors[i as usize];
                }
            });
        }
        _ => return Err(malformed("unexpected index sample type")),
    }

    Ok(DecodedImage {
        pixels,
        width,
        height,
        format: PixelFormat::Rgba8Unorm,
        bit_depth: 8,
        channels: 3,
        icc,
        source_format: "TIFF",
        alpha_opaque: false, // set by `decode` after the final buffer is built
        downscaled_from: None,
        source_mips: None,
        layout: None,
        animation: None,
    })
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
        extra: &[u16],
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
        if !extra.is_empty() {
            // One or two SHORTs pack inline into the entry's value field — all any test needs.
            assert!(extra.len() <= 2);
            let packed = extra
                .iter()
                .enumerate()
                .fold(0u32, |acc, (i, &e)| acc | (u32::from(e) << (16 * i)));
            tags.push((338, 3, extra.len() as u32, packed)); // ExtraSamples
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

    /// An uncompressed single-strip palette TIFF (photometric 3) with `map` as its `ColorMap`,
    /// in either byte order — `build` cannot place a tag that large, so this lays out its own.
    fn palette(w: u32, h: u32, bits: u16, map: &[u16], data: &[u8], big_endian: bool) -> Vec<u8> {
        let u16b = |v: u16| {
            if big_endian {
                v.to_be_bytes()
            } else {
                v.to_le_bytes()
            }
        };
        let u32b = |v: u32| {
            if big_endian {
                v.to_be_bytes()
            } else {
                v.to_le_bytes()
            }
        };
        // A SHORT value sits left-justified in the entry's 4-byte field.
        let short = |v: u16| -> u32 { u32::from_ne_bytes([u16b(v)[0], u16b(v)[1], 0, 0]) };
        let n = 10usize;
        let after_ifd = 8 + 2 + n * 12 + 4;
        let data_off = after_ifd + map.len() * 2;
        let entries: [(u16, u16, u32, u32); 10] = [
            (256, 3, 1, short(w as u16)),
            (257, 3, 1, short(h as u16)),
            (258, 3, 1, short(bits)),
            (259, 3, 1, short(1)),
            (262, 3, 1, short(3)), // RGBPalette
            (273, 4, 1, u32::from_ne_bytes(u32b(data_off as u32))),
            (277, 3, 1, short(1)),
            (278, 3, 1, short(h as u16)),
            (279, 4, 1, u32::from_ne_bytes(u32b(data.len() as u32))),
            (
                320,
                3,
                map.len() as u32,
                u32::from_ne_bytes(u32b(after_ifd as u32)),
            ),
        ];
        let mut b = if big_endian {
            b"MM\0\x2a".to_vec()
        } else {
            b"II\x2a\0".to_vec()
        };
        b.extend_from_slice(&u32b(8));
        b.extend_from_slice(&u16b(n as u16));
        for (tag, typ, cnt, val) in entries {
            b.extend_from_slice(&u16b(tag));
            b.extend_from_slice(&u16b(typ));
            b.extend_from_slice(&u32b(cnt));
            b.extend_from_slice(&val.to_ne_bytes());
        }
        b.extend_from_slice(&u32b(0));
        for &v in map {
            b.extend_from_slice(&u16b(v));
        }
        assert_eq!(b.len(), data_off);
        b.extend_from_slice(data);
        b
    }

    /// Palette TIFFs open — they used to fail in both the `tiff` crate and the `image` fallback —
    /// with each pixel the colour its index names, at every index width: 8-bit, packed 4- and
    /// 1-bit (on an odd width, so rows end mid-byte and are padded), 16-bit, in both byte orders.
    /// The 16-bit map scales to 8 bits with rounding, and a map with nothing above 255 is read as
    /// the 8-bit map very old writers stored.
    #[test]
    fn palette_tiffs_map_indices_to_colours() {
        // Entry i: red i*257, green (255-i)*257, blue a fixed 0x1234 (→ 18 at 8 bits).
        let map16 = |entries: usize| -> Vec<u16> {
            let channel = |f: &dyn Fn(usize) -> u16| (0..entries).map(f).collect::<Vec<_>>();
            [
                channel(&|i| (i % 256) as u16 * 257),
                channel(&|i| (255 - i % 256) as u16 * 257),
                channel(&|_| 0x1234),
            ]
            .concat()
        };
        let colour = |i: usize| [(i % 256) as u8, (255 - i % 256) as u8, 18, 255];

        for big_endian in [false, true] {
            // 8-bit, 3x2.
            let idx = [0u8, 1, 255, 128, 7, 200];
            let out = run(&palette(3, 2, 8, &map16(256), &idx, big_endian));
            assert_eq!((out.channels, out.format), (3, PixelFormat::Rgba8Unorm));
            let expected: Vec<u8> = idx.iter().flat_map(|&i| colour(i as usize)).collect();
            assert_eq!(out.pixels, expected, "8-bit, big-endian {big_endian}");

            // 4-bit, 3x2: two indices a byte, high nibble first; each row padded to a byte.
            let idx = [[1usize, 15, 7], [0, 9, 2]];
            let data = [0x1f, 0x70, 0x09, 0x20];
            let out = run(&palette(3, 2, 4, &map16(16), &data, big_endian));
            let expected: Vec<u8> = idx.iter().flatten().flat_map(|&i| colour(i)).collect();
            assert_eq!(out.pixels, expected, "4-bit, big-endian {big_endian}");

            // 1-bit, 9x1: eight indices in the first byte, the ninth in the next one's top bit.
            let data = [0b1010_0110, 0b1000_0000];
            let out = run(&palette(9, 1, 1, &map16(2), &data, big_endian));
            let bits = [1usize, 0, 1, 0, 0, 1, 1, 0, 1];
            let expected: Vec<u8> = bits.iter().flat_map(|&i| colour(i)).collect();
            assert_eq!(out.pixels, expected, "1-bit, big-endian {big_endian}");

            // 16-bit indices, 2x1.
            let data: Vec<u8> = [300u16, 65535]
                .iter()
                .flat_map(|&i| {
                    if big_endian {
                        i.to_be_bytes()
                    } else {
                        i.to_le_bytes()
                    }
                })
                .collect();
            let out = run(&palette(2, 1, 16, &map16(65536), &data, big_endian));
            let expected: Vec<u8> = [300usize, 65535].iter().flat_map(|&i| colour(i)).collect();
            assert_eq!(out.pixels, expected, "16-bit, big-endian {big_endian}");
        }

        // A legacy 8-bit map: every value at most 255, read as is rather than scaled down to ~0.
        let legacy: Vec<u16> = [vec![200u16, 10], vec![100, 20], vec![50, 30]].concat();
        let out = run(&palette(2, 1, 1, &legacy, &[0b0100_0000], false));
        assert_eq!(out.pixels, vec![200, 100, 50, 255, 10, 20, 30, 255]);
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
        let out = run(&build(1, 1, RGB, 4, 8, &[0], &[255, 255, 255, 128]));
        assert_eq!(out.channels, 4);
        assert_eq!(out.pixels, vec![255, 255, 255, 128]);

        // A file that already says "unassociated alpha" needs no patch and decodes the same.
        let declared = build(1, 1, RGB, 4, 8, &[2], &[255, 255, 255, 128]);
        assert!(matches!(extra_sample_as_alpha(&declared), Cow::Borrowed(_)));
        assert_eq!(run(&declared).pixels, vec![255, 255, 255, 128]);
    }

    /// Extra channels beyond the first alpha must not shift the readout.
    ///
    /// `colortype()` reports RGBA for RGB + alpha + spot (Photoshop's layout: SamplesPerPixel
    /// = 5, ExtraSamples = [unassociated alpha, unspecified]) while the file stores five
    /// samples per pixel. The `tiff` crate trims the unnamed extras during readout, and
    /// `expand` strides by the named count — this test pins that pairing: if a crate upgrade
    /// ever hands back the file's full layout instead, striding it by the named count would
    /// silently smear every pixel after the first diagonally.
    #[test]
    fn extra_channels_beyond_alpha_do_not_smear() {
        let out = run(&build(
            2,
            1,
            RGB,
            5,
            8,
            &[2, 0],
            &[10, 20, 30, 40, 99, 50, 60, 70, 80, 111],
        ));
        assert_eq!(
            out.channels, 4,
            "alpha is still seen through the spot channel"
        );
        assert_eq!(
            out.pixels,
            vec![10, 20, 30, 40, 50, 60, 70, 80],
            "second pixel must come from the 5-sample stride, spot samples skipped"
        );

        // Extras that are not alpha at all: plain RGB plus two unspecified channels.
        let out = run(&build(
            2,
            1,
            RGB,
            5,
            8,
            &[0, 0],
            &[10, 20, 30, 98, 99, 50, 60, 70, 110, 111],
        ));
        assert_eq!(out.channels, 3, "no alpha among the extras");
        assert_eq!(out.pixels, vec![10, 20, 30, 255, 50, 60, 70, 255]);
    }

    /// Greyscale + alpha opens at all.
    ///
    /// Two samples under `BlackIsZero` are reported as `Multiband { num_samples: 2 }`, which the
    /// `image` crate maps to `Unknown(16)` and refuses outright — the file simply would not open.
    #[test]
    fn grayscale_plus_alpha_decodes() {
        for extra in [&[2u16][..], &[0], &[]] {
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

    /// A compressed, many-strip TIFF big enough to be decoded on several threads comes back
    /// exactly as written — 8-bit RGB and 16-bit RGBA alike, so both the widening and the
    /// pass-through land on the strip boundaries correctly. Odd dimensions and a strip height that
    /// does not divide the image leave a short last strip, the case the layout check exists for.
    #[test]
    fn many_strip_compressed_tiff_reads_in_parallel() {
        use tiff::encoder::{colortype, Compression, TiffEncoder};

        let (w, h) = (701u32, 523u32);
        let rgb: Vec<u8> = (0..w * h * 3)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 11) as u8)
            .collect();
        let rgba16: Vec<u16> = (0..w * h * 4)
            .map(|i| i.wrapping_mul(2_654_435_761) as u16)
            .collect();

        let mut file = Cursor::new(Vec::new());
        let mut enc = TiffEncoder::new(&mut file)
            .unwrap()
            .with_compression(Compression::Lzw);
        let mut img = enc.new_image::<colortype::RGB8>(w, h).unwrap();
        img.rows_per_strip(7).unwrap();
        img.write_data(&rgb).unwrap();
        let out = run(&file.into_inner());
        assert_eq!((out.width, out.height, out.channels), (w, h, 3));
        let expected: Vec<u8> = rgb
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect();
        assert!(out.pixels == expected, "8-bit RGB");

        let mut file = Cursor::new(Vec::new());
        let mut enc = TiffEncoder::new(&mut file)
            .unwrap()
            .with_compression(Compression::Lzw);
        let mut img = enc.new_image::<colortype::RGBA16>(w, h).unwrap();
        img.rows_per_strip(5).unwrap();
        img.write_data(&rgba16).unwrap();
        let out = run(&file.into_inner());
        assert_eq!(out.format, PixelFormat::Rgba16Unorm);
        assert!(out.pixels == crate::to_ne_bytes(&rgba16), "16-bit RGBA");
    }

    /// 16-bit TIFFs keep 16 bits. The `image` adapter special-cased only float, so everything
    /// else went through `to_rgba8()` and lost half its depth while `bit_depth` still said 16.
    #[test]
    fn sixteen_bit_is_not_narrowed() {
        let le = |v: [u16; 4]| -> Vec<u8> { v.iter().flat_map(|s| s.to_le_bytes()).collect() };
        let out = run(&build(1, 1, RGB, 4, 16, &[2], &le([65535, 4660, 0, 32768])));
        assert_eq!(out.format, PixelFormat::Rgba16Unorm);
        assert_eq!(out.bit_depth, 16);
        let s: Vec<u16> = out
            .pixels
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_ne_bytes(*c))
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
            &[],
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
        let out = run(&build(1, 1, RGB, 4, 8, &[1], &[128, 0, 0, 128]));
        assert_eq!(
            out.pixels,
            vec![255, 0, 0, 128],
            "colour restored to its straight value"
        );

        // The same samples labelled unassociated are already straight and must not be touched.
        let out = run(&build(1, 1, RGB, 4, 8, &[2], &[128, 0, 0, 128]));
        assert_eq!(out.pixels, vec![128, 0, 0, 128]);

        // Fully transparent premultiplied pixels carry no colour to recover; they must not
        // divide by zero.
        let out = run(&build(1, 1, RGB, 4, 8, &[1], &[0, 0, 0, 0]));
        assert_eq!(out.pixels, vec![0, 0, 0, 0]);
    }

    /// Colour types this module does not own still decode, via the `image` fallback.
    #[test]
    fn unowned_color_types_fall_back_to_the_image_crate() {
        // CMYK (photometric 5): full cyan ink. `image` converts it; we must not swallow it.
        let out = run(&build(1, 1, 5, 4, 8, &[], &[255, 0, 0, 0]));
        assert_eq!(out.source_format, "TIFF");
        assert_eq!(out.pixels[..3], [0, 255, 255], "cyan survives the fallback");

        // Plain RGB and plain grey stay on the native path and report honest channel counts.
        assert_eq!(
            run(&build(2, 1, RGB, 3, 8, &[], &[255, 0, 0, 0, 255, 0])).channels,
            3
        );
        assert_eq!(
            run(&build(2, 1, BLACK_IS_ZERO, 1, 8, &[], &[64, 192])).channels,
            1
        );
    }

    /// The byte patch rewrites two bytes and nothing else, and only in a classic TIFF.
    #[test]
    fn extra_sample_patch_is_minimal_and_safe() {
        let original = build(1, 1, RGB, 4, 8, &[0], &[1, 2, 3, 4]);
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
            &build(1, 1, RGB, 3, 8, &[], &[1, 2, 3])[..], // no ExtraSamples tag
            &build(1, 1, RGB, 4, 8, &[2], &[1, 2, 3, 4])[..], // already alpha
            &original[..20],                              // truncated mid-IFD
            b"II\x2a\x00",                                // header only
            b"\x89PNG\r\n\x1a\n",                         // not a TIFF
            b"",
        ] {
            assert!(matches!(extra_sample_as_alpha(bytes), Cow::Borrowed(_)));
        }
    }
}
