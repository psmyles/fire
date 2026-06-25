//! Uniform decode core: `bytes -> (pixels, format, bit depth, optional ICC)`.
//!
//! All format backends live behind the single [`decode`] entry point so the daemon
//! never sees per-format detail. Routing is by magic bytes:
//!   - PSD  -> psd_sdk (C++ FFI)        : merged composite, 8-bit RGBA (+ICC)
//!   - EXR  -> `exr` crate              : 32-bit float RGBA (linear/HDR)
//!   - else -> `image` crate            : PNG/JPEG/TGA/TIFF/GIF/BMP/WebP/HDR (+ICC)
//!
//! ICC profiles are extracted here; the lcms2 transform into the working space is
//! applied by [`icc`]. Images larger than the GPU max texture dimension are
//! CPU-downscaled to fit ([`downscale`]).
//!
//! Note (deviation from the plan's letter): the `image` crate is used for the LDR hot
//! path (PNG/JPEG) rather than zune. The architecture treats decode *speed* as
//! non-critical (cold-start is the latency enemy, and decode runs off the main thread
//! on a worker, never blocking the window), so swapping in zune is a clean follow-up
//! optimization of this same interface, not a correctness concern.

use std::io::Cursor;
use std::path::Path;

mod downscale;

/// Pixel layout of a decoded image. Drives the wgpu texture format and whether the
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
    /// If the image was downscaled to fit the GPU max texture dimension, the original
    /// (width, height) before downscaling; the pixel inspector notes this (§6).
    pub downscaled_from: Option<(u32, u32)>,
}

/// Options controlling a decode.
#[derive(Debug, Clone, Copy)]
pub struct DecodeOptions {
    /// Max texture dimension from the live wgpu device `Limits`; images larger than
    /// this on either axis are CPU-downscaled to fit before upload (§6).
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
    /// An FFI backend (psd_sdk/lcms2) failed; surfaced so the daemon survives.
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
    Image,
}

fn sniff(bytes: &[u8]) -> Backend {
    if bytes.starts_with(b"8BPS") {
        Backend::Psd
    } else if bytes.starts_with(&[0x76, 0x2f, 0x31, 0x01]) {
        // OpenEXR magic number.
        Backend::Exr
    } else {
        // PNG/JPEG/TGA/TIFF/GIF/BMP/WebP/Radiance-HDR all go through the image crate,
        // which sniffs them itself.
        Backend::Image
    }
}

/// Decode an in-memory image, choosing a backend by magic bytes.
pub fn decode(
    bytes: &[u8],
    _ext_hint: Option<&str>,
    opts: &DecodeOptions,
) -> Result<DecodedImage, DecodeError> {
    let mut img = match sniff(bytes) {
        Backend::Psd => decode_psd(bytes)?,
        Backend::Exr => decode_exr(bytes)?,
        Backend::Image => decode_image(bytes)?,
    };

    // Honor an embedded ICC profile by transforming into the working space (Phase 2).
    if opts.honor_icc {
        icc::apply(&mut img);
    }

    // Fit within the GPU's max texture dimension.
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

/// PNG/JPEG/TGA/TIFF/GIF/BMP/WebP/Radiance-HDR via the `image` crate. Extracts the
/// embedded ICC profile (where the format carries one) and keeps float sources as
/// 32-bit float RGBA (HDR).
fn decode_image(bytes: &[u8]) -> Result<DecodedImage, DecodeError> {
    use image::DynamicImage;

    let reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| DecodeError::Other(e.to_string()))?;
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
        channels: 4,
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
        WebP => "WebP",
        Hdr => "Radiance HDR",
        _ => "image",
    }
}

// --- ICC ---------------------------------------------------------------------

mod icc {
    //! ICC handling. Phase 2 step here extracts the profile (already stored on
    //! [`DecodedImage::icc`] by the backends, used by the status bar). The lcms2
    //! transform into the working space is wired in the next step.

    use crate::DecodedImage;

    /// Apply the embedded ICC transform in place, if present. Currently a no-op beyond
    /// the extraction the backends already did; the lcms2 transform lands next.
    pub fn apply(_img: &mut DecodedImage) {
        // TODO(Phase 2, next step): build an lcms2 Transform from img.icc into the
        // working space (sRGB for 8-bit, linear for float) and apply it in place,
        // inside catch_unwind, best-effort (fall back to sRGB assumption on failure).
    }
}
