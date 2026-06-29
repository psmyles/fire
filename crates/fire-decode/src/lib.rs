//! Uniform decode core: `bytes -> (pixels, format, bit depth, optional ICC)`.
//!
//! All format backends live behind the single [`decode`] entry point so callers
//! never see per-format detail. Routing is by magic bytes (and, for camera raw, the file
//! extension, since the many TIFF-structured raws can't be told from plain TIFF by header):
//!   - PSD            -> psd_sdk (C++ FFI) : merged composite, 8-bit RGBA (+ICC)
//!   - EXR            -> `exr` crate       : 32-bit float RGBA (linear/HDR)
//!   - HEIC/HEIF/AVIF -> libheif (C FFI)   : 8-bit RGBA, or 16-bit RGBA for HDR (+ICC)
//!   - camera raw     -> [`raw`] preview   : extract the embedded JPEG, decode via zune
//!   - zune-supported -> **zune** (hot path): PNG/JPEG/HDR/BMP/QOI/PPM/WebP/farbfeld/JXL
//!   - else           -> `image` crate     : TIFF/GIF/TGA/ICO (formats zune doesn't decode)
//!
//! **Decode speed is the project's primary metric** (time-to-first-pixel), so the common
//! formats run through zune with [`DecoderOptions::new_fast`] (platform intrinsics +
//! unsafe fast paths enabled). zune output is normalized to interleaved RGBA in the
//! source bit depth (8/16/float). The `image` crate is kept only as a fallback for the
//! handful of formats zune has no decoder for (TIFF/GIF/TGA), where decode speed is far
//! less important.
//!
//! ICC profiles are extracted here; the lcms2 transform into the working space is
//! applied by [`icc`]. Images larger than the caller's `max_dim` are CPU-downscaled to
//! fit ([`downscale`]).

use std::io::Cursor;
use std::path::Path;

mod downscale;
mod raw;

/// Upper bound on a decoded source dimension. zune's default cap is 16384, which would
/// reject any image larger than that on an axis before we ever get a chance to downscale
/// it to the caller's `max_dim` — so we raise the cap well past any realistic image while
/// still rejecting absurd (corrupt/decode-bomb) headers in the billions.
const MAX_DECODE_DIM: usize = 1 << 17; // 131072

/// Pixel layout of a decoded image. Drives the per-format CPU sampling path and whether the
/// HDR exposure/tonemap path applies (float = HDR, linear working space).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// 8-bit per channel, sRGB working space (the common LDR case).
    Rgba8Unorm,
    /// 16-bit unsigned per channel, sRGB working space.
    Rgba16Unorm,
    /// 16-bit half-float per channel, linear working space (HDR).
    Rgba16Float,
    /// 32-bit float per channel, linear working space (HDR).
    Rgba32Float,
}

impl PixelFormat {
    /// Whether this format carries HDR/linear data (exposure + tonemap apply).
    pub fn is_hdr(self) -> bool {
        matches!(self, PixelFormat::Rgba16Float | PixelFormat::Rgba32Float)
    }

    /// Bytes per RGBA pixel for this format.
    pub fn bytes_per_pixel(self) -> usize {
        match self {
            PixelFormat::Rgba8Unorm => 4,
            PixelFormat::Rgba16Unorm | PixelFormat::Rgba16Float => 8,
            PixelFormat::Rgba32Float => 16,
        }
    }
}

/// A successfully decoded image, normalized to RGBA in `format`'s layout.
#[derive(Debug, Clone)]
pub struct DecodedImage {
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    /// Bits per channel of the *source* (for the status bar), independent of `format`.
    pub bit_depth: u8,
    /// Channel count of the source (1=gray, 3=RGB, 4=RGBA, ...).
    pub channels: u8,
    /// Embedded ICC profile bytes, if the backend surfaced one.
    pub icc: Option<Vec<u8>>,
    /// Human-readable source format name for the status bar (e.g. "PNG", "OpenEXR").
    pub source_format: &'static str,
    /// If the image was downscaled to fit `DecodeOptions::max_dim`, the original
    /// (width, height) before downscaling; the pixel inspector notes this (§6).
    pub downscaled_from: Option<(u32, u32)>,
}

/// Options controlling a decode.
#[derive(Debug, Clone, Copy)]
pub struct DecodeOptions {
    /// Max decoded dimension on either axis — a CPU/RAM guard, not a GPU texture limit.
    /// Images larger than this on either axis are CPU-downscaled to fit (§6). An RGBA8
    /// bitmap at 16384² is ~1 GiB; float HDR is 4×.
    pub max_dim: u32,
    /// Whether to parse and honor embedded ICC profiles via lcms2.
    pub honor_icc: bool,
}

impl Default for DecodeOptions {
    fn default() -> Self {
        Self { max_dim: 16384, honor_icc: true }
    }
}

/// Decode failure modes.
#[derive(Debug)]
pub enum DecodeError {
    /// Could not determine the format from magic bytes or extension.
    UnknownFormat,
    /// The backend rejected the data as malformed.
    Malformed(String),
    /// An FFI backend (psd_sdk/lcms2) failed; surfaced so the viewer survives.
    Ffi(String),
    /// I/O or unexpected backend error.
    Other(String),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::UnknownFormat => write!(f, "unknown or unsupported image format"),
            DecodeError::Malformed(m) => write!(f, "malformed image: {m}"),
            DecodeError::Ffi(m) => write!(f, "decoder FFI error: {m}"),
            DecodeError::Other(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Which backend handles a given byte stream.
enum Backend {
    Psd,
    Exr,
    /// HEIC/HEIF/AVIF via libheif; carries the status-bar label for the detected brand.
    Heif(&'static str),
    /// A zune-decodable format; carries zune's detected format for status-bar naming.
    Zune(zune_image::codecs::ImageFormat),
    /// A camera-raw file; carries the status-bar label for the detected raw family. The
    /// embedded JPEG preview is extracted ([`raw`]) and decoded through the zune path.
    Raw(&'static str),
    Image,
}

/// Detect an ISOBMFF (HEIF-family) stream by its `ftyp` box brand and map it to a
/// status-bar label. The layout is `[u32 box-size][b"ftyp"][u32 major-brand][...]`, so
/// the major brand sits at bytes 8..12. Returns `None` for non-HEIF input.
fn heif_label(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() < 12 || &bytes[4..8] != b"ftyp" {
        return None;
    }
    match &bytes[8..12] {
        b"avif" | b"avis" => Some("AVIF"),
        // HEVC-in-HEIF brands.
        b"heic" | b"heix" | b"heim" | b"heis" | b"hevc" | b"hevx" | b"hevm" | b"hevs" => {
            Some("HEIC")
        }
        // Generic HEIF (codec announced in compatible brands; libheif sorts it out).
        b"mif1" | b"msf1" | b"mif2" => Some("HEIF"),
        _ => None,
    }
}

fn sniff(bytes: &[u8], ext: Option<&str>) -> Backend {
    use zune_core::bytestream::ZCursor;
    use zune_image::codecs::{guess_format, ImageFormat};

    if bytes.starts_with(b"8BPS") {
        // PSD: psd_sdk gives a better composite than zune-psd, and carries the ICC.
        Backend::Psd
    } else if bytes.starts_with(&[0x76, 0x2f, 0x31, 0x01]) {
        // OpenEXR magic number — handled by the `exr` crate (zune has no EXR decoder).
        Backend::Exr
    } else if let Some(label) = heif_label(bytes) {
        // HEIC/HEIF/AVIF — the ISOBMFF `ftyp` brands; decoded by libheif.
        Backend::Heif(label)
    } else if let Some(label) = raw::label(bytes, ext) {
        // Camera raw (CR2/CR3/NEF/ARW/RAF/DNG/…): display the embedded JPEG preview. Sniffed
        // before the zune/`image` fallback because TIFF-structured raws share TIFF's magic
        // and must not be handed to the `image` crate as ordinary TIFFs.
        Backend::Raw(label)
    } else if let Some((fmt, _)) = guess_format(ZCursor::new(bytes)) {
        // zune recognizes it (PNG/JPEG/HDR/BMP/QOI/PPM/WebP/farbfeld/JXL): the fast path.
        if fmt == ImageFormat::Unknown {
            Backend::Image
        } else {
            Backend::Zune(fmt)
        }
    } else {
        // zune doesn't sniff it (TIFF/GIF/TGA/ICO): fall back to the image crate.
        Backend::Image
    }
}

/// Decode an in-memory image, choosing a backend by magic bytes.
pub fn decode(
    bytes: &[u8],
    ext_hint: Option<&str>,
    opts: &DecodeOptions,
) -> Result<DecodedImage, DecodeError> {
    let mut img = match sniff(bytes, ext_hint) {
        Backend::Psd => decode_psd(bytes)?,
        Backend::Exr => decode_exr(bytes)?,
        Backend::Heif(label) => decode_heif(bytes, label)?,
        Backend::Raw(label) => decode_raw(bytes, label)?,
        Backend::Zune(fmt) => decode_zune(bytes, fmt)?,
        Backend::Image => decode_image(bytes, ext_hint)?,
    };

    // Honor an embedded ICC profile by transforming into the working space.
    if opts.honor_icc {
        icc::apply(&mut img);
    }

    // Fit within the caller's max dimension (RAM guard).
    downscale::to_fit(&mut img, opts.max_dim);

    Ok(img)
}

/// Convenience wrapper: read a file and decode it (used by the decode worker).
pub fn decode_path(path: &Path, opts: &DecodeOptions) -> Result<DecodedImage, DecodeError> {
    let bytes = std::fs::read(path).map_err(|e| DecodeError::Other(e.to_string()))?;
    let ext = path.extension().and_then(|e| e.to_str());
    decode(&bytes, ext, opts)
}

// --- backends ---------------------------------------------------------------

/// PSD via psd_sdk (C++ FFI). Runs inside catch_unwind so a Rust-side panic in the
/// thin wrapper cannot escape; the C++ side additionally guards against C++ exceptions.
fn decode_psd(bytes: &[u8]) -> Result<DecodedImage, DecodeError> {
    let result = std::panic::catch_unwind(|| psd_sdk_sys::decode_psd(bytes))
        .map_err(|_| DecodeError::Ffi("psd_sdk panicked".into()))?;
    let psd = result.map_err(|e| DecodeError::Malformed(e.to_string()))?;
    Ok(DecodedImage {
        pixels: psd.rgba8,
        width: psd.width,
        height: psd.height,
        format: PixelFormat::Rgba8Unorm,
        bit_depth: psd.bits_per_channel.min(255) as u8,
        channels: psd.channels.min(255) as u8,
        icc: psd.icc,
        source_format: "PSD",
        downscaled_from: None,
    })
}

/// OpenEXR via the `exr` crate → 32-bit float RGBA (linear/HDR).
fn decode_exr(bytes: &[u8]) -> Result<DecodedImage, DecodeError> {
    use exr::prelude::*;

    struct Buf {
        width: usize,
        pixels: Vec<[f32; 4]>,
    }

    let image = read()
        .no_deep_data()
        .largest_resolution_level()
        .rgba_channels(
            |size, _| Buf {
                width: size.width(),
                pixels: vec![[0.0f32; 4]; size.width() * size.height()],
            },
            |buf: &mut Buf, pos, (r, g, b, a): (f32, f32, f32, f32)| {
                let i = pos.y() * buf.width + pos.x();
                buf.pixels[i] = [r, g, b, a];
            },
        )
        .first_valid_layer()
        .all_attributes()
        .from_buffered(Cursor::new(bytes))
        .map_err(|e| DecodeError::Malformed(e.to_string()))?;

    let size = image.layer_data.size;
    let buf = image.layer_data.channel_data.pixels;
    let mut pixels = Vec::with_capacity(buf.pixels.len() * 16);
    for px in &buf.pixels {
        for c in px {
            pixels.extend_from_slice(&c.to_ne_bytes());
        }
    }
    Ok(DecodedImage {
        pixels,
        width: size.width() as u32,
        height: size.height() as u32,
        format: PixelFormat::Rgba32Float,
        bit_depth: 32,
        channels: 4,
        icc: None,
        source_format: "OpenEXR",
        downscaled_from: None,
    })
}

/// HEIC/HEIF/AVIF via libheif (C FFI: libde265 for HEVC, dav1d for AV1). Runs inside
/// catch_unwind so a panic in the thin wrapper cannot escape; libheif itself reports
/// malformed input as an error code rather than crashing.
///
/// 8-bit sources come back as `Rgba8Unorm`; HDR (10/12-bit) sources as `Rgba16Unorm`
/// scaled to full range. We treat the decoded values as display-encoded (SDR), so a
/// true-HDR (PQ/HLG) HEIF will display without tonemapping — acceptable for v1, and most
/// HEIC (phone photos) is 8-bit SDR, often Display-P3, whose ICC the [`icc`] pass honors.
fn decode_heif(bytes: &[u8], label: &'static str) -> Result<DecodedImage, DecodeError> {
    let result = std::panic::catch_unwind(|| heif_sys::decode_heif(bytes))
        .map_err(|_| DecodeError::Ffi("libheif panicked".into()))?;
    let img = result.map_err(|e| DecodeError::Ffi(e.to_string()))?;

    let (format, bit_depth) = if img.is_16bit {
        (PixelFormat::Rgba16Unorm, img.bit_depth)
    } else {
        (PixelFormat::Rgba8Unorm, 8)
    };
    Ok(DecodedImage {
        pixels: img.pixels,
        width: img.width,
        height: img.height,
        format,
        bit_depth,
        // Report the source channel count for the status bar (4 with alpha, else 3).
        channels: if img.has_alpha { 4 } else { 3 },
        icc: img.icc,
        source_format: label,
        downscaled_from: None,
    })
}

/// Camera raw via embedded-preview extraction ([`raw`]). The raw container is not decoded;
/// instead we locate the largest embedded JPEG preview the camera wrote and decode *that*
/// through the normal zune JPEG path, so ICC handling, downscale, and the rest apply
/// unchanged. The pixels are the camera's own rendering (white-balanced, 8-bit) — the right
/// trade for a fast viewer, and we apply the file's EXIF orientation so portrait shots are
/// upright. Developing the sensor mosaic (demosaic/color matrices) is out of scope (§6).
///
/// Pure-Rust and bounds-checked end to end, but the worker pool still wraps the whole decode
/// in `catch_unwind` as a backstop, same as every other backend.
fn decode_raw(bytes: &[u8], label: &'static str) -> Result<DecodedImage, DecodeError> {
    let preview = raw::find_preview(bytes)
        .ok_or_else(|| DecodeError::Malformed("raw file has no embedded JPEG preview".into()))?;
    // The preview is a JPEG; reuse the zune path (ICC + bit-depth + naming) then re-label it
    // as the raw family and orient it.
    let mut img = decode_zune(preview.jpeg, zune_image::codecs::ImageFormat::JPEG)?;
    raw::apply_orientation(&mut img, preview.orientation);
    img.source_format = label;
    Ok(img)
}

/// The hot path: zune for PNG/JPEG/HDR/BMP/QOI/PPM/WebP/farbfeld/JPEG-XL. Decoded with
/// the speed-first options, normalized to interleaved RGBA in the source bit depth, and
/// carrying the embedded ICC profile where the format exposes one.
///
/// v1 takes the first frame only (#18: animated GIF → frame 0).
fn decode_zune(
    bytes: &[u8],
    fmt: zune_image::codecs::ImageFormat,
) -> Result<DecodedImage, DecodeError> {
    use zune_core::bit_depth::BitDepth;
    use zune_core::bytestream::ZCursor;
    use zune_core::colorspace::ColorSpace;
    use zune_core::options::DecoderOptions;
    use zune_image::image::Image;

    // Speed is the project's top metric: enable platform intrinsics + unsafe fast paths.
    // Raise the dimension guard well past zune's 16384 default so large sources decode
    // (the downscale pass shrinks anything beyond the caller's max_dim afterwards).
    let opts = DecoderOptions::new_fast()
        .set_max_width(MAX_DECODE_DIM)
        .set_max_height(MAX_DECODE_DIM);

    let mut image =
        Image::read(ZCursor::new(bytes), opts).map_err(|e| DecodeError::Malformed(e.to_string()))?;

    // Source characteristics for the status bar, captured before we normalize to RGBA.
    let src_channels = image.colorspace().num_components().min(255) as u8;
    let icc = image.metadata().icc_chunk().cloned();

    // Normalize every colorspace (RGB/Luma/LumaA/CMYK/BGR/…) to interleaved RGBA. This
    // preserves the source bit depth (8/16/float) and adds an opaque alpha where missing.
    image
        .convert_color(ColorSpace::RGBA)
        .map_err(|e| DecodeError::Other(e.to_string()))?;

    let (width, height) = image.dimensions();
    let frame = image
        .frames_ref()
        .first()
        .ok_or_else(|| DecodeError::Malformed("image has no frames".into()))?;

    let (pixels, pixel_format, bit_depth) = match image.depth() {
        BitDepth::Eight => (frame.flatten::<u8>(), PixelFormat::Rgba8Unorm, 8u8),
        BitDepth::Sixteen => {
            // The CPU shader reads Rgba16Unorm back as native-endian u16.
            let u16s = frame.flatten::<u16>();
            let mut out = Vec::with_capacity(u16s.len() * 2);
            for v in u16s {
                out.extend_from_slice(&v.to_ne_bytes());
            }
            (out, PixelFormat::Rgba16Unorm, 16)
        }
        BitDepth::Float32 => {
            // Float sources (Radiance .hdr) are linear/HDR → exposure + tonemap apply.
            let f32s = frame.flatten::<f32>();
            let mut out = Vec::with_capacity(f32s.len() * 4);
            for v in f32s {
                out.extend_from_slice(&v.to_ne_bytes());
            }
            (out, PixelFormat::Rgba32Float, 32)
        }
        // BitDepth::Unknown and any future variant.
        _ => return Err(DecodeError::Malformed("unsupported bit depth".into())),
    };

    Ok(DecodedImage {
        pixels,
        width: width as u32,
        height: height as u32,
        format: pixel_format,
        bit_depth,
        channels: src_channels,
        icc,
        source_format: zune_format_name(fmt),
        downscaled_from: None,
    })
}

fn zune_format_name(f: zune_image::codecs::ImageFormat) -> &'static str {
    use zune_image::codecs::ImageFormat::*;
    match f {
        JPEG => "JPEG",
        PNG => "PNG",
        PPM => "PPM",
        PSD => "PSD",
        Farbfeld => "Farbfeld",
        QOI => "QOI",
        JPEG_XL => "JPEG XL",
        HDR => "Radiance HDR",
        BMP => "BMP",
        WEBP => "WebP",
        _ => "image",
    }
}

/// Fallback for the formats zune has no decoder for: TIFF/GIF/TGA/ICO via the `image` crate.
/// Extracts the embedded ICC profile (where the format carries one) and keeps float
/// sources as 32-bit float RGBA (HDR). Decode speed here is not critical (rare formats).
///
/// TGA carries no start-of-file magic (only an optional end-of-file `TRUEVISION-XFILE.`
/// footer), so `with_guessed_format` can't detect it from content. Fall back to the file
/// extension for any format content-sniffing misses.
fn decode_image(bytes: &[u8], ext_hint: Option<&str>) -> Result<DecodedImage, DecodeError> {
    use image::DynamicImage;

    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| DecodeError::Other(e.to_string()))?;
    if reader.format().is_none() {
        if let Some(fmt) = ext_hint.and_then(image::ImageFormat::from_extension) {
            reader.set_format(fmt);
        }
    }
    let format = reader.format();
    let mut decoder = reader
        .into_decoder()
        .map_err(|e| DecodeError::Malformed(e.to_string()))?;
    // ICC must be queried before the decoder is consumed by from_decoder.
    let icc = {
        use image::ImageDecoder;
        decoder.icc_profile().ok().flatten()
    };
    let dynimg = DynamicImage::from_decoder(decoder)
        .map_err(|e| DecodeError::Malformed(e.to_string()))?;
    let width = dynimg.width();
    let height = dynimg.height();
    // Report the source channel count for the status bar and alpha-aware UI (RGBA icon,
    // alpha-channel button, checker backdrop). We always normalize to RGBA below, so the
    // pixel buffer is 4-wide regardless — but a 24-bit TGA / RGB TIFF carries no alpha and
    // must not be presented as if it did.
    let src_channels = dynimg.color().channel_count();

    let (pixels, fmt, bit_depth) = match &dynimg {
        // Float sources (Radiance HDR, float TIFF) stay 32-bit float / linear (HDR).
        DynamicImage::ImageRgb32F(_) | DynamicImage::ImageRgba32F(_) => {
            let rgba = dynimg.to_rgba32f();
            let mut bytes = Vec::with_capacity(rgba.as_raw().len() * 4);
            for f in rgba.as_raw() {
                bytes.extend_from_slice(&f.to_ne_bytes());
            }
            (bytes, PixelFormat::Rgba32Float, 32)
        }
        _ => (dynimg.to_rgba8().into_raw(), PixelFormat::Rgba8Unorm, 8),
    };

    Ok(DecodedImage {
        pixels,
        width,
        height,
        format: fmt,
        bit_depth,
        channels: src_channels,
        icc,
        source_format: format.map(format_name).unwrap_or("image"),
        downscaled_from: None,
    })
}

fn format_name(f: image::ImageFormat) -> &'static str {
    use image::ImageFormat::*;
    match f {
        Png => "PNG",
        Jpeg => "JPEG",
        Gif => "GIF",
        Bmp => "BMP",
        Tiff => "TIFF",
        Tga => "TGA",
        Ico => "ICO",
        WebP => "WebP",
        Hdr => "Radiance HDR",
        _ => "image",
    }
}

// --- ICC ---------------------------------------------------------------------

mod icc {
    //! ICC handling via lcms2 (C FFI). The backends extract the embedded profile onto
    //! [`DecodedImage::icc`]; here we transform the pixels into the sRGB working space so
    //! a Display-P3 / Adobe-RGB / etc. image displays with correct color on the (sRGB)
    //! surface. Best-effort: any failure (bad profile, unsupported layout) leaves the
    //! pixels untouched, i.e. falls back to assuming the data is already sRGB.

    use crate::{DecodedImage, PixelFormat};

    /// Transform `img`'s pixels from their embedded ICC profile into sRGB, in place.
    ///
    /// No-op when there is no profile, for HDR/float data (linear working space; our
    /// float backends never carry an ICC), or for non-RGB profiles (our pixels are RGBA,
    /// so a CMYK/Gray profile would be misapplied — we assume sRGB instead).
    pub fn apply(img: &mut DecodedImage) {
        let Some(icc) = img.icc.clone() else { return };
        if img.format.is_hdr() {
            return;
        }
        // FFI safety boundary (§6/§15): a malformed profile must never unwind into and
        // crash the viewer process. catch_unwind + best-effort: on any failure we keep
        // the original pixels (sRGB assumption).
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            transform_to_srgb(img, &icc);
        }));
    }

    fn transform_to_srgb(img: &mut DecodedImage, icc: &[u8]) {
        use lcms2::{ColorSpaceSignature, Flags, Intent, PixelFormat as Fmt, Profile, Transform};

        let Ok(src) = Profile::new_icc(icc) else { return };
        // Only RGB(A) source profiles map cleanly onto our RGBA pixels.
        if src.color_space() != ColorSpaceSignature::RgbData {
            return;
        }
        let dst = Profile::new_srgb();
        // Perceptual: the usual choice for displaying photographic images. COPY_ALPHA so
        // the alpha channel passes through untouched (lcms only transforms color).
        let intent = Intent::Perceptual;

        match img.format {
            PixelFormat::Rgba8Unorm => {
                let t: Transform<[u8; 4], [u8; 4]> = match Transform::new_flags(
                    &src,
                    Fmt::RGBA_8,
                    &dst,
                    Fmt::RGBA_8,
                    intent,
                    Flags::COPY_ALPHA,
                ) {
                    Ok(t) => t,
                    Err(_) => return,
                };
                if let Ok(px) = bytemuck::try_cast_slice_mut::<u8, [u8; 4]>(&mut img.pixels) {
                    t.transform_in_place(px);
                }
            }
            PixelFormat::Rgba16Unorm => {
                let t: Transform<[u16; 4], [u16; 4]> = match Transform::new_flags(
                    &src,
                    Fmt::RGBA_16,
                    &dst,
                    Fmt::RGBA_16,
                    intent,
                    Flags::COPY_ALPHA,
                ) {
                    Ok(t) => t,
                    Err(_) => return,
                };
                // 16-bit pixels are native-endian u16 bytes; cast may fail on alignment,
                // in which case we skip (sRGB assumption) rather than panic.
                if let Ok(px) = bytemuck::try_cast_slice_mut::<u8, [u16; 4]>(&mut img.pixels) {
                    t.transform_in_place(px);
                }
            }
            // Float is handled by the is_hdr() early-out in apply().
            _ => {}
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn img8(pixels: Vec<u8>) -> DecodedImage {
            let n = (pixels.len() / 4) as u32;
            DecodedImage {
                pixels,
                width: n,
                height: 1,
                format: PixelFormat::Rgba8Unorm,
                bit_depth: 8,
                channels: 4,
                icc: None,
                source_format: "test",
                downscaled_from: None,
            }
        }

        #[test]
        fn no_icc_is_noop() {
            let mut img = img8(vec![10, 20, 30, 40, 200, 150, 100, 255]);
            let before = img.pixels.clone();
            apply(&mut img);
            assert_eq!(img.pixels, before);
        }

        #[test]
        fn srgb_profile_near_identity_and_preserves_alpha() {
            let icc = lcms2::Profile::new_srgb().icc().unwrap();
            let mut img = img8(vec![10, 20, 30, 40, 200, 150, 100, 255, 0, 128, 255, 77]);
            img.icc = Some(icc);
            let before = img.pixels.clone();
            apply(&mut img);
            // sRGB -> sRGB is (near) identity; allow tiny rounding on the color channels.
            for (i, (a, b)) in img.pixels.iter().zip(&before).enumerate() {
                assert!(
                    (*a as i32 - *b as i32).abs() <= 2,
                    "channel {i} drifted: {a} vs {b}"
                );
            }
            // COPY_ALPHA: alpha bytes (every 4th) preserved exactly.
            assert_eq!([img.pixels[3], img.pixels[7], img.pixels[11]], [40, 255, 77]);
        }

        #[test]
        fn malformed_profile_is_safe_noop() {
            let mut img = img8(vec![10, 20, 30, 40]);
            img.icc = Some(vec![0, 1, 2, 3, 4, 5]); // not a valid ICC profile
            let before = img.pixels.clone();
            apply(&mut img); // must not panic
            assert_eq!(img.pixels, before);
        }

        /// Proves the embedded transfer curve is actually applied (not just channel
        /// shuffling): a profile identical to sRGB except with a *linear* TRC means a
        /// mid-gray 128 is linear-light 0.5, which sRGB-encodes to ~188. So the gray must
        /// move substantially upward after the transform.
        #[test]
        fn linear_rgb_profile_applies_tone_curve() {
            use lcms2::{CIExyY, CIExyYTRIPLE, Profile, ToneCurve};

            let d65 = CIExyY { x: 0.3127, y: 0.3290, Y: 1.0 };
            let primaries = CIExyYTRIPLE {
                Red: CIExyY { x: 0.640, y: 0.330, Y: 1.0 },
                Green: CIExyY { x: 0.300, y: 0.600, Y: 1.0 },
                Blue: CIExyY { x: 0.150, y: 0.060, Y: 1.0 },
            };
            let linear = ToneCurve::new(1.0);
            let profile = Profile::new_rgb(&d65, &primaries, &[&linear, &linear, &linear]).unwrap();
            let icc = profile.icc().unwrap();

            let mut img = img8(vec![128, 128, 128, 200]);
            img.icc = Some(icc);
            apply(&mut img);

            // Linear 0.5 -> sRGB ~= 188. Allow a generous window; the point is "much higher".
            assert!(
                img.pixels[0] >= 175 && img.pixels[0] <= 200,
                "linear 128 should sRGB-encode to ~188, got {}",
                img.pixels[0]
            );
            // Stays neutral gray and alpha is untouched.
            assert_eq!(img.pixels[0], img.pixels[1]);
            assert_eq!(img.pixels[1], img.pixels[2]);
            assert_eq!(img.pixels[3], 200);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode `img` to the given format with the `image` crate (test fixtures only).
    fn encode(img: &image::DynamicImage, fmt: image::ImageFormat) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        img.write_to(&mut buf, fmt).expect("encode fixture");
        buf.into_inner()
    }

    /// A lossless RGBA PNG must come back through the zune hot path byte-for-byte.
    #[test]
    fn zune_png_rgba_roundtrip() {
        let mut src = image::RgbaImage::new(2, 2);
        src.put_pixel(0, 0, image::Rgba([255, 0, 0, 255]));
        src.put_pixel(1, 0, image::Rgba([0, 255, 0, 255]));
        src.put_pixel(0, 1, image::Rgba([0, 0, 255, 128]));
        src.put_pixel(1, 1, image::Rgba([10, 20, 30, 40]));
        let bytes = encode(&image::DynamicImage::ImageRgba8(src), image::ImageFormat::Png);

        let out = decode(&bytes, Some("png"), &DecodeOptions::default()).unwrap();
        assert_eq!((out.width, out.height), (2, 2));
        assert_eq!(out.format, PixelFormat::Rgba8Unorm);
        assert_eq!(out.source_format, "PNG");
        assert_eq!(out.channels, 4);
        assert_eq!(
            out.pixels,
            vec![255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 128, 10, 20, 30, 40]
        );
    }

    /// A grayscale PNG decodes to Luma then expands to RGBA: gray replicated, opaque alpha.
    /// The reported source channel count stays 1 (for the status bar).
    #[test]
    fn zune_grayscale_png_expands_to_rgba() {
        let mut src = image::GrayImage::new(2, 1);
        src.put_pixel(0, 0, image::Luma([40]));
        src.put_pixel(1, 0, image::Luma([200]));
        let bytes = encode(&image::DynamicImage::ImageLuma8(src), image::ImageFormat::Png);

        let out = decode(&bytes, Some("png"), &DecodeOptions::default()).unwrap();
        assert_eq!((out.width, out.height), (2, 1));
        assert_eq!(out.format, PixelFormat::Rgba8Unorm);
        assert_eq!(out.channels, 1);
        assert_eq!(out.pixels, vec![40, 40, 40, 255, 200, 200, 200, 255]);
    }

    /// A 16-bit PNG stays 16-bit (precision preserved for the inspector / HDR pipeline).
    #[test]
    fn zune_png16_stays_16bit() {
        let mut src = image::ImageBuffer::<image::Rgba<u16>, _>::new(1, 1);
        src.put_pixel(0, 0, image::Rgba([0xFFFF, 0x8000, 0x0001, 0xFFFF]));
        let bytes = encode(&image::DynamicImage::ImageRgba16(src), image::ImageFormat::Png);

        let out = decode(&bytes, Some("png"), &DecodeOptions::default()).unwrap();
        assert_eq!(out.format, PixelFormat::Rgba16Unorm);
        assert_eq!(out.bit_depth, 16);
        // Native-endian u16 RGBA.
        let px: Vec<u16> = out
            .pixels
            .chunks_exact(2)
            .map(|c| u16::from_ne_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(px, vec![0xFFFF, 0x8000, 0x0001, 0xFFFF]);
    }

    /// A solid-color JPEG decodes through zune to RGBA8 with the right dims (lossy, so we
    /// only assert the color is approximately right, and source is reported as 3-channel).
    #[test]
    fn zune_jpeg_decodes() {
        let src = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            16,
            16,
            image::Rgb([220, 30, 40]),
        ));
        let bytes = encode(&src, image::ImageFormat::Jpeg);

        let out = decode(&bytes, Some("jpg"), &DecodeOptions::default()).unwrap();
        assert_eq!((out.width, out.height), (16, 16));
        assert_eq!(out.format, PixelFormat::Rgba8Unorm);
        assert_eq!(out.source_format, "JPEG");
        assert_eq!(out.channels, 3);
        let [r, g, b, a] = [out.pixels[0], out.pixels[1], out.pixels[2], out.pixels[3]];
        assert!(r > 200 && g < 70 && b < 80, "got {r},{g},{b}");
        assert_eq!(a, 255);
    }

    /// An ICO routes to the `image`-crate fallback (zune can't sniff it) and decodes to
    /// RGBA8 with the right dims and status-bar label. ICO embeds a PNG or BMP per entry;
    /// the `image` crate picks the largest. Proves the AVIF/HEIF work didn't need to touch
    /// ICO — it already works through the fallback.
    #[test]
    fn ico_decodes_via_fallback() {
        let mut src = image::RgbaImage::new(4, 4);
        src.put_pixel(0, 0, image::Rgba([200, 30, 40, 255]));
        src.put_pixel(3, 3, image::Rgba([10, 20, 30, 128]));
        let bytes = encode(&image::DynamicImage::ImageRgba8(src), image::ImageFormat::Ico);

        let out = decode(&bytes, Some("ico"), &DecodeOptions::default()).unwrap();
        assert_eq!((out.width, out.height), (4, 4));
        assert_eq!(out.format, PixelFormat::Rgba8Unorm);
        assert_eq!(out.source_format, "ICO");
        assert_eq!([out.pixels[0], out.pixels[1], out.pixels[2], out.pixels[3]], [200, 30, 40, 255]);
    }

    /// A camera-raw file routes to the preview extractor: a synthetic little-endian TIFF
    /// whose IFD points at an embedded full-size JPEG (with Orientation 8 / rotate-90°-CCW)
    /// decodes to the preview's pixels, re-labeled as the raw family, oriented upright. This
    /// exercises the whole wiring: ext-driven sniff -> decode_raw -> zune -> orientation.
    #[test]
    fn raw_decodes_embedded_preview_via_ext() {
        // Embedded preview: a 6(w)x2(h) JPEG. Orientation 8 swaps axes -> 2x6 displayed.
        let preview = encode(
            &image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(6, 2, image::Rgb([200, 40, 60]))),
            image::ImageFormat::Jpeg,
        );

        let entries: u16 = 3;
        let ifd_off = 8u32;
        let ifd_len = 2 + entries as usize * 12 + 4;
        let jpeg_off = ifd_off as usize + ifd_len;

        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"II");
        tiff.extend_from_slice(&42u16.to_le_bytes());
        tiff.extend_from_slice(&ifd_off.to_le_bytes());
        tiff.extend_from_slice(&entries.to_le_bytes());
        let mut entry = |tag: u16, typ: u16, n: u32, val: u32| {
            tiff.extend_from_slice(&tag.to_le_bytes());
            tiff.extend_from_slice(&typ.to_le_bytes());
            tiff.extend_from_slice(&n.to_le_bytes());
            tiff.extend_from_slice(&val.to_le_bytes());
        };
        entry(0x0112, 3, 1, 8); // Orientation = 8 (rotate 90° CCW)
        entry(0x0201, 4, 1, jpeg_off as u32);
        entry(0x0202, 4, 1, preview.len() as u32);
        tiff.extend_from_slice(&0u32.to_le_bytes());
        tiff.extend_from_slice(&preview);

        let out = decode(&tiff, Some("nef"), &DecodeOptions::default()).unwrap();
        assert_eq!(out.source_format, "Nikon NEF");
        assert_eq!(out.format, PixelFormat::Rgba8Unorm);
        // Orientation 8 swaps the 6x2 preview to 2x6.
        assert_eq!((out.width, out.height), (2, 6));
        // The camera's red is preserved through the JPEG round-trip.
        let [r, g, b] = [out.pixels[0], out.pixels[1], out.pixels[2]];
        assert!(r > 180 && g < 90 && b < 100, "got {r},{g},{b}");
    }

    /// TGA has no start-of-file magic, so content sniffing can't identify it — the decoder
    /// must lean on the file extension. Regression for TGA files failing to open at all.
    #[test]
    fn tga_decodes_via_extension_hint() {
        let mut src = image::RgbaImage::new(2, 1);
        src.put_pixel(0, 0, image::Rgba([200, 30, 40, 255]));
        src.put_pixel(1, 0, image::Rgba([10, 220, 60, 255]));
        let bytes = encode(&image::DynamicImage::ImageRgba8(src), image::ImageFormat::Tga);

        // Without an extension hint the format is unidentifiable (the original bug).
        assert!(decode(&bytes, None, &DecodeOptions::default()).is_err());

        // With the hint it decodes — and the hint is matched case-insensitively (.TGA).
        for ext in ["tga", "TGA"] {
            let out = decode(&bytes, Some(ext), &DecodeOptions::default()).unwrap();
            assert_eq!((out.width, out.height), (2, 1));
            assert_eq!(out.source_format, "TGA");
            assert_eq!(&out.pixels[0..4], &[200, 30, 40, 255]);
        }
    }

    /// A 24-bit (RGB) TGA carries no alpha; it must report 3 channels so the viewer doesn't
    /// present it with a checker backdrop / alpha-channel UI. Regression: `decode_image`
    /// hardcoded `channels: 4`, so every TGA looked like it had an alpha channel.
    #[test]
    fn rgb_tga_reports_three_channels() {
        let mut src = image::RgbImage::new(2, 1);
        src.put_pixel(0, 0, image::Rgb([200, 30, 40]));
        src.put_pixel(1, 0, image::Rgb([10, 220, 60]));
        let bytes = encode(&image::DynamicImage::ImageRgb8(src), image::ImageFormat::Tga);

        let out = decode(&bytes, Some("tga"), &DecodeOptions::default()).unwrap();
        assert_eq!(out.channels, 3, "24-bit RGB TGA must not report an alpha channel");
        // Still normalized to RGBA pixels with opaque alpha for the GPU upload.
        assert_eq!(&out.pixels[0..4], &[200, 30, 40, 255]);

        // A genuine 32-bit RGBA TGA still reports 4 channels.
        let mut rgba = image::RgbaImage::new(1, 1);
        rgba.put_pixel(0, 0, image::Rgba([1, 2, 3, 128]));
        let bytes = encode(&image::DynamicImage::ImageRgba8(rgba), image::ImageFormat::Tga);
        let out = decode(&bytes, Some("tga"), &DecodeOptions::default()).unwrap();
        assert_eq!(out.channels, 4, "32-bit RGBA TGA must report an alpha channel");
    }

    /// Corrupt input must surface an error, never panic (FFI-free path, but the viewer
    /// relies on this being a clean `Err`).
    #[test]
    fn garbage_input_errors() {
        let bytes = b"\x89PNG\r\n\x1a\n garbage that is not a real png body";
        let r = decode(bytes, Some("png"), &DecodeOptions::default());
        assert!(r.is_err());
    }
}
