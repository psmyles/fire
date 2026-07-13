//! Uniform decode core: `bytes -> (pixels, format, bit depth, optional ICC)`.
//!
//! All format backends live behind the single [`decode`] entry point so callers
//! never see per-format detail. Routing is by magic bytes (and, for camera raw, the file
//! extension, since the many TIFF-structured raws can't be told from plain TIFF by header):
//!   - PSD            -> psd_sdk (C++ FFI) : merged composite, 8-bit RGBA (+ICC)
//!   - EXR            -> `exr` crate       : 32-bit float RGBA (linear/HDR)
//!   - HEIC/HEIF/AVIF -> libheif (C FFI)   : 8-bit RGBA, or 16-bit RGBA for HDR (+ICC)
//!   - camera raw     -> [`raw`] preview   : extract the embedded JPEG, decode via zune
//!   - GIF            -> `image` crate     : every frame (animated GIF plays; see [`Animation`])
//!   - Radiance HDR   -> `image` crate     : 32-bit float RGBA (linear/HDR); see [`decode_hdr`]
//!   - PNG            -> `image` crate     : RGBA8/RGBA16 (+ICC); see [`decode_png`]
//!   - zune-supported -> **zune** (hot path): JPEG/BMP/QOI/PPM/WebP/farbfeld/JXL
//!   - else           -> `image` crate     : TIFF/TGA/ICO (formats zune doesn't decode)
//!
//! **Decode speed is the project's primary metric** (time-to-first-pixel), so the common
//! formats run through zune with [`DecoderOptions::new_fast`] (platform intrinsics +
//! unsafe fast paths enabled). zune output is normalized to interleaved RGBA in the
//! source bit depth (8/16/float). The `image` crate is kept as a fallback for the
//! handful of formats zune has no decoder for (TIFF/GIF/TGA), where decode speed is far
//! less important — and, deliberately, for Radiance HDR and PNG, where its decoders
//! measured faster than zune's (and, for HDR, correct where zune-hdr is not); see
//! [`decode_hdr`] / [`decode_png`].
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

/// Upper bound on the *total* pixel buffer an animated source may hold, across all frames.
/// Every GIF frame is a full RGBA canvas ([`AnimationFrame`]), so memory is
/// `frames × width × height × 4` — bounding the dimensions alone leaves the frame count free to
/// multiply them. 1 GiB is far past any real animation (a 640×480 GIF gets ~870 frames) while
/// keeping a crafted one from exhausting RAM.
const MAX_ANIMATION_BYTES: usize = 1 << 30; // 1 GiB

/// Hard ceiling on animation frames, independent of [`MAX_ANIMATION_BYTES`]: a tiny canvas
/// makes the byte budget effectively unbounded, and every frame still costs a `Vec` and a
/// timer tick.
const MAX_ANIMATION_FRAMES: usize = 10_000;

/// Upper bound on the pixel buffer a single decoded image may allocate, in bytes. Checked against
/// `width × height × bytes_per_pixel`, because **the product is what gets allocated** — a per-axis
/// cap alone bounds nothing useful: GIF's dimensions are `u16`, so a 65535×65535 GIF sits far under
/// [`MAX_DECODE_DIM`] on both axes and still asks for 17 GiB.
///
/// 4 GiB clears the largest images this viewer is meant to open (the 216-MP scan cited in
/// [`decode_png`] is ~1.7 GiB at 16-bit) while refusing the decode bombs.
const MAX_DECODE_BYTES: usize = 4 << 30; // 4 GiB

/// Reject a header whose declared size would allocate more than we are willing to — **before**
/// anything is allocated from it. `bytes_per_pixel` is that of the buffer the caller is about to
/// allocate (i.e. the *normalized RGBA* output: 4, 8, or 16), not the source's own layout.
///
/// This guard is not redundant with the `catch_unwind` that wraps every decode, and cannot be
/// replaced by it: `catch_unwind` catches *panics*, and a `Vec` allocation that fails does not
/// panic — it calls `handle_alloc_error`, which **aborts the process**. A crafted 20-byte header
/// claiming billions of pixels has to be turned away here; nothing downstream can catch it.
///
/// Applied by every backend that sizes a buffer from parsed dimensions *and* can see those
/// dimensions before allocating: PNG, GIF, Radiance HDR, OpenEXR. The zune hot path cannot —
/// `zune_image::Image::read` decodes in one shot and only reports dimensions afterwards — so it
/// relies on zune's own per-axis caps ([`MAX_DECODE_DIM`]) and remains bounded only by the product
/// of those; see `decode_zune`.
fn check_dims(
    width: usize,
    height: usize,
    bytes_per_pixel: usize,
    what: &str,
) -> Result<(), DecodeError> {
    if width > MAX_DECODE_DIM || height > MAX_DECODE_DIM {
        return Err(DecodeError::Malformed(format!(
            "{what} dimensions {width}x{height} exceed the {MAX_DECODE_DIM} per-axis decode guard"
        )));
    }
    let bytes = width
        .checked_mul(height)
        .and_then(|px| px.checked_mul(bytes_per_pixel));
    match bytes {
        Some(b) if b <= MAX_DECODE_BYTES => Ok(()),
        _ => Err(DecodeError::Malformed(format!(
            "{what} {width}x{height} needs more than the {MAX_DECODE_BYTES}-byte decode guard"
        ))),
    }
}

/// Every file extension fire can open, lower-case.
///
/// **The** list — the viewer's Open-dialog filter (`win.rs`) and its folder-navigation membership
/// test (`folder.rs`) both read it from here rather than keeping their own. They used to keep their
/// own, and the two had already drifted apart: a `.qoi` was reachable with the arrow keys but
/// invisible in the Open dialog. The installer's per-format associations (`installer/fire.iss`) are
/// a fourth consumer that *cannot* import this — it is an Inno Setup script — so a test below reads
/// the `.iss` and asserts it registers exactly this set.
///
/// Note this is a convenience for *naming* files, not the routing decision: [`decode`] sniffs magic
/// bytes and will happily open a supported image with the wrong extension (or none).
pub const SUPPORTED_EXTENSIONS: &[&str] = &[
    // Still formats, in the order the module doc lists the backends.
    "png", "jpg", "jpeg", "jpe", "jfif", "gif", "bmp", "dib", "tif", "tiff", "webp", "ico", "tga",
    "qoi", "ppm", "pgm", "pbm", "pnm", "ff", "jxl", "hdr", "exr", "psd", "psb", "heic", "heif",
    "avif", //
    // Camera raw (embedded-preview decode). Mirrors `raw::EXT_LABELS`, which is what actually
    // routes them — `raw_extensions_are_all_listed` keeps the two honest.
    "cr2", "cr3", "crw", "nef", "nrw", "arw", "srf", "sr2", "raf", "orf", "rw2", "pef", "srw",
    "dng", "x3f", "3fr", "fff", "iiq", "erf", "mrw", "dcr", "kdc", "mef", "mos", "rwl", "gpr",
    "raw",
];

/// Whether `ext` (with no leading dot, any case) is one fire can open. See
/// [`SUPPORTED_EXTENSIONS`].
pub fn is_supported_extension(ext: &str) -> bool {
    let lower = ext.to_ascii_lowercase();
    SUPPORTED_EXTENSIONS.contains(&lower.as_str())
}

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
    /// The source's true channel count (1=gray, 2=gray+A, 3=RGB, 4=RGBA), for the status bar and
    /// the RGB↔RGBA toolbar icon. Reported faithfully even when an alpha channel is fully opaque —
    /// a 32-bit PNG screenshot still reads "RGBA" and keeps an inspectable alpha channel; whether
    /// that alpha actually carries transparency is a separate signal ([`alpha_opaque`](Self::alpha_opaque)).
    pub channels: u8,
    /// Whether a declared alpha channel (`channels` 2 or 4) is entirely opaque — every sample
    /// fully opaque, so there is no transparency to composite. Set by [`decode`] from a scan of the
    /// final (post-ICC, post-downscale) buffer; the viewer uses it to skip the default checker
    /// backdrop for, e.g., a screenshot whose alpha is uniformly `0xff`, while still reporting the
    /// true format and letting the user isolate the (all-white) alpha channel. `false` whenever
    /// there is no alpha channel.
    pub alpha_opaque: bool,
    /// Embedded ICC profile bytes, if the backend surfaced one.
    pub icc: Option<Vec<u8>>,
    /// Human-readable source format name for the status bar (e.g. "PNG", "OpenEXR").
    pub source_format: &'static str,
    /// If the image was downscaled to fit `DecodeOptions::max_dim`, the original
    /// (width, height) before downscaling; the pixel inspector notes this (§6).
    pub downscaled_from: Option<(u32, u32)>,
    /// Playback timing/pixels for an animated source (animated GIF). `None` for a still image —
    /// the common case, so the still path is untouched. When `Some`, `pixels` above is frame 0
    /// (shown immediately) and [`Animation::frames`] holds the full sequence for the viewer to
    /// cycle through. See [`Animation`].
    pub animation: Option<Animation>,
}

/// One frame of an animated image: a full, ready-to-display RGBA canvas plus how long to show it.
#[derive(Debug, Clone)]
pub struct AnimationFrame {
    /// Full-canvas RGBA pixels for this frame, already composited over the prior frames by the
    /// decoder (GIF disposal handled), in the parent [`DecodedImage`]'s `format`/dimensions — so
    /// the viewer just swaps the texture with no per-frame compositing.
    pub pixels: Vec<u8>,
    /// How long this frame is displayed before advancing, in milliseconds.
    pub delay_ms: u32,
}

/// Multi-frame animation for an animated source (currently animated GIF). Present on a
/// [`DecodedImage`] only when the source has more than one frame.
#[derive(Debug, Clone)]
pub struct Animation {
    /// Every frame in play order (frame 0 included, matching [`DecodedImage::pixels`]). Each is a
    /// complete canvas at the image's dimensions, so playback is a plain texture swap per frame.
    pub frames: Vec<AnimationFrame>,
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
///
/// There is deliberately no `UnknownFormat` variant: [`sniff`] always resolves to *some* backend
/// (`Backend::Image` is the catch-all), so unrecognized input is not a routing failure — it is a
/// backend rejecting bytes it cannot parse, and comes back as [`Malformed`](Self::Malformed). A
/// variant that nothing can construct is a promise the type does not keep.
#[derive(Debug)]
pub enum DecodeError {
    /// The backend rejected the data as malformed — including input that is not an image at all.
    Malformed(String),
    /// An FFI backend (psd_sdk/lcms2) failed; surfaced so the viewer survives.
    Ffi(String),
    /// I/O or unexpected backend error.
    Other(String),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
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
    /// A GIF (still or animated); decoded frame-by-frame via the `image` crate so an animated
    /// GIF can play. Sniffed separately from [`Backend::Image`] to reach the multi-frame path.
    Gif,
    /// Radiance HDR (`.hdr`/`.pic`); decoded by the `image` crate, *not* zune — see
    /// [`decode_hdr`] for why zune-hdr is avoided.
    Hdr,
    /// PNG; decoded by the `image` crate, *not* zune — see [`decode_png`] for why
    /// zune-png is avoided.
    Png,
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
    } else if bytes.starts_with(b"GIF8") {
        // GIF (b"GIF87a"/b"GIF89a"): route to the dedicated multi-frame decoder so an animated
        // GIF plays. zune has no GIF decoder anyway, so this only pre-empts the `image` fallback.
        Backend::Gif
    } else if let Some((fmt, _)) = guess_format(ZCursor::new(bytes)) {
        // zune recognizes it (JPEG/BMP/QOI/PPM/WebP/farbfeld/JXL): the fast path — except
        // HDR and PNG, which zune sniffs for us but the `image` crate decodes (its decoders
        // measured faster than zune's for both; see decode_hdr / decode_png).
        match fmt {
            ImageFormat::Unknown => Backend::Image,
            // zune's sniff covers both the `#?RADIANCE` and `#?RGBE` magics for us.
            ImageFormat::HDR => Backend::Hdr,
            ImageFormat::PNG => Backend::Png,
            _ => Backend::Zune(fmt),
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
        Backend::Gif => decode_gif(bytes)?,
        Backend::Hdr => decode_hdr(bytes)?,
        Backend::Png => decode_png(bytes)?,
        Backend::Zune(fmt) => decode_zune(bytes, fmt)?,
        Backend::Image => decode_image(bytes, ext_hint)?,
    };

    // Honor an embedded ICC profile by transforming into the working space.
    if opts.honor_icc {
        icc::apply(&mut img);
    }

    // Fit within the caller's max dimension (RAM guard).
    downscale::to_fit(&mut img, opts.max_dim);

    // Flag a declared-but-fully-opaque alpha channel. The container format (`channels`) is left
    // truthful — a 32-bit PNG screenshot still reports RGBA and keeps an inspectable alpha — but
    // the viewer reads this to avoid defaulting to the checker backdrop when there is no actual
    // transparency to reveal. Scanned on the final (post-ICC, post-downscale) buffer, i.e. exactly
    // what is displayed; the scan short-circuits on the first transparent sample.
    img.alpha_opaque = matches!(img.channels, 2 | 4) && alpha_is_opaque(&img);

    Ok(img)
}

/// Whether the normalized RGBA buffer is fully opaque (every alpha sample at its max). A cheap
/// linear scan over just the alpha lane that short-circuits on the first transparent sample;
/// run once per decode off the UI thread (see [`decode`]).
fn alpha_is_opaque(img: &DecodedImage) -> bool {
    let px = &img.pixels;
    match img.format {
        // 8-bit: 4 bytes/px, alpha is byte 3; opaque == 0xff.
        PixelFormat::Rgba8Unorm => px.chunks_exact(4).all(|p| p[3] == 0xff),
        // 16-bit unorm (native-endian u16): 8 bytes/px, alpha is bytes 6..8; opaque == 0xffff.
        PixelFormat::Rgba16Unorm => px.chunks_exact(8).all(|p| p[6] == 0xff && p[7] == 0xff),
        // 16-bit half-float: opaque == 1.0 == 0x3c00. No decode path emits this today, but keep
        // the lane handling exhaustive over PixelFormat.
        PixelFormat::Rgba16Float => {
            px.chunks_exact(8).all(|p| u16::from_ne_bytes([p[6], p[7]]) == 0x3c00)
        }
        // 32-bit float (linear/HDR): 16 bytes/px, alpha is the 4th f32. Opaque == 1.0; values
        // above 1.0 count as opaque, NaN does not (keeping the alpha channel is the safe default).
        PixelFormat::Rgba32Float => {
            px.chunks_exact(16).all(|p| f32::from_ne_bytes([p[12], p[13], p[14], p[15]]) >= 1.0)
        }
    }
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
        alpha_opaque: false, // set by `decode` after the final buffer is built
        downscaled_from: None,
        animation: None,
    })
}

/// OpenEXR via the `exr` crate → 32-bit float RGBA (linear/HDR).
///
/// The headers are parsed on their own first, because the `rgba_channels` size closure below
/// cannot fail: it allocates 16 bytes per declared pixel and hands the buffer back by value, so a
/// crafted header would abort the process there ([`check_dims`]) with nothing able to intercept
/// it. Reading the metadata separately is the only place a dimension check *can* refuse.
fn decode_exr(bytes: &[u8]) -> Result<DecodedImage, DecodeError> {
    use exr::prelude::*;

    struct Buf {
        width: usize,
        pixels: Vec<[f32; 4]>,
    }

    let meta = exr::meta::MetaData::read_from_buffered(Cursor::new(bytes), false)
        .map_err(|e| DecodeError::Malformed(e.to_string()))?;
    for header in &meta.headers {
        let size = header.layer_size;
        // 16 bytes/px: the closure's `[f32; 4]` buffer, and again for the `Vec<u8>` built from it.
        check_dims(size.width(), size.height(), 16, "OpenEXR")?;
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
        alpha_opaque: false, // set by `decode` after the final buffer is built
        downscaled_from: None,
        animation: None,
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
        alpha_opaque: false, // set by `decode` after the final buffer is built
        downscaled_from: None,
        animation: None,
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

/// Radiance HDR via the `image` crate → 32-bit float RGBA (linear/HDR).
///
/// Deliberately *off* the zune hot path, for two reasons measured on real files:
/// - **Correctness:** zune-hdr (≤ 0.5.2, and upstream `dev` as of 2026-07) computes the RGBE
///   scale `2^(E-128)` with a shift masked `& 31`, so any pixel with `|E-128| >= 32` wraps —
///   near-black values (E ≤ 96) come back exactly 2^32 too bright, rendering as bright
///   blue/green patches in dark regions.
/// - **Speed:** the `image` crate decodes the same files ~2× faster than zune-hdr, so this
///   routing also serves the time-to-first-pixel metric.
///
/// Non-strict mode accepts the `#?RGBE` signature variant and old signature-less `.pic`
/// files that the strict `#?RADIANCE` check would reject. Radiance carries no ICC profile;
/// the data is linear RGB, so the HDR exposure/tonemap path applies downstream.
fn decode_hdr(bytes: &[u8]) -> Result<DecodedImage, DecodeError> {
    use image::ImageDecoder;

    let decoder = image::codecs::hdr::HdrDecoder::with_strictness(Cursor::new(bytes), false)
        .map_err(|e| DecodeError::Malformed(e.to_string()))?;
    // Constructed directly (not via `ImageReader`), so the reader's default memory limits don't
    // apply — the header guard is ours to make, exactly as in `decode_png`. `into_rgba32f` below
    // allocates 16 bytes per declared pixel.
    let (w, h) = decoder.dimensions();
    check_dims(w as usize, h as usize, 16, "Radiance HDR")?;

    let dynimg = image::DynamicImage::from_decoder(decoder)
        .map_err(|e| DecodeError::Malformed(e.to_string()))?;
    let (width, height) = (dynimg.width(), dynimg.height());

    let rgba = dynimg.into_rgba32f();
    let mut pixels = Vec::with_capacity(rgba.as_raw().len() * 4);
    for f in rgba.as_raw() {
        pixels.extend_from_slice(&f.to_ne_bytes());
    }

    Ok(DecodedImage {
        pixels,
        width,
        height,
        format: PixelFormat::Rgba32Float,
        bit_depth: 32,
        channels: 3, // RGBE is always RGB; the alpha lane is added by normalization
        icc: None,
        source_format: "Radiance HDR",
        alpha_opaque: false, // set by `decode` after the final buffer is built
        downscaled_from: None,
        animation: None,
    })
}

/// PNG via the `image` crate → RGBA8, or RGBA16 for 16-bit sources (precision preserved
/// for the inspector / HDR pipeline). Extracts the embedded ICC profile.
///
/// Deliberately *off* the zune hot path: on large real-world PNGs the `image` crate's
/// `png`+`fdeflate` stack decodes ~1.8× faster than zune-png end-to-end (measured 2026-07
/// on 8192×4096 game textures: ~190 ms vs ~340 ms including RGBA normalization), and the
/// gap is in the core decode, not wrapper overhead. Constructed directly rather than via
/// `image::ImageReader` so the reader's default memory limits don't reject large-but-real
/// images (a 216-MP scan trips them); the [`MAX_DECODE_DIM`] header guard here and the
/// caller's `max_dim` downscale are the actual bomb/RAM guards, matching the zune path.
fn decode_png(bytes: &[u8]) -> Result<DecodedImage, DecodeError> {
    use image::{DynamicImage, ImageDecoder};

    let mut decoder = image::codecs::png::PngDecoder::new(Cursor::new(bytes))
        .map_err(|e| DecodeError::Malformed(e.to_string()))?;
    let (width, height) = decoder.dimensions();
    // 16-bit sources are kept at 16 bits (8 bytes/px RGBA); everything else normalizes to RGBA8.
    // Mirrors the `is_16bit` split below — the buffer this sizes is the one it allocates.
    let out_bpp = match decoder.color_type() {
        image::ColorType::L16
        | image::ColorType::La16
        | image::ColorType::Rgb16
        | image::ColorType::Rgba16 => 8,
        _ => 4,
    };
    check_dims(width as usize, height as usize, out_bpp, "PNG")?;
    // Source channel count (status bar / alpha-aware UI) and ICC must be read before
    // `from_decoder` consumes the decoder. Palette sources already report their expanded
    // RGB/RGBA color type, matching what zune reported.
    let src_channels = decoder.color_type().channel_count();
    let icc = decoder.icc_profile().ok().flatten();

    let dynimg = DynamicImage::from_decoder(decoder)
        .map_err(|e| DecodeError::Malformed(e.to_string()))?;

    let is_16bit = matches!(
        dynimg,
        DynamicImage::ImageLuma16(_)
            | DynamicImage::ImageLumaA16(_)
            | DynamicImage::ImageRgb16(_)
            | DynamicImage::ImageRgba16(_)
    );
    let (pixels, format, bit_depth) = if is_16bit {
        // The CPU shader reads Rgba16Unorm back as native-endian u16.
        let rgba = dynimg.into_rgba16();
        let mut out = Vec::with_capacity(rgba.as_raw().len() * 2);
        for v in rgba.as_raw() {
            out.extend_from_slice(&v.to_ne_bytes());
        }
        (out, PixelFormat::Rgba16Unorm, 16u8)
    } else {
        (dynimg.into_rgba8().into_raw(), PixelFormat::Rgba8Unorm, 8)
    };

    Ok(DecodedImage {
        pixels,
        width,
        height,
        format,
        bit_depth,
        channels: src_channels,
        icc,
        source_format: "PNG",
        alpha_opaque: false, // set by `decode` after the final buffer is built
        downscaled_from: None,
        animation: None,
    })
}

/// The hot path: zune for JPEG/BMP/QOI/PPM/WebP/farbfeld/JPEG-XL. Decoded with
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
    //
    // NOTE: unlike the other backends this is a *per-axis* cap only — zune offers no total-size
    // option, and `Image::read` decodes in one shot, so there is no point at which we can see the
    // dimensions and still refuse ([`check_dims`] is unreachable from here). A crafted file that
    // stays under the cap on both axes but multiplies out huge (a 65535×65535 JPEG ≈ 17 GiB) can
    // therefore still force an allocation that aborts the process. Closing it means probing each
    // zune format's header before decoding; until then this cap is the only bound.
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
            // Float sources are linear/HDR → exposure + tonemap apply.
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
        alpha_opaque: false, // set by `decode` after the final buffer is built
        downscaled_from: None,
        animation: None,
    })
}

fn zune_format_name(f: zune_image::codecs::ImageFormat) -> &'static str {
    use zune_image::codecs::ImageFormat::*;
    match f {
        JPEG => "JPEG",
        PPM => "PPM",
        PSD => "PSD",
        Farbfeld => "Farbfeld",
        QOI => "QOI",
        JPEG_XL => "JPEG XL",
        BMP => "BMP",
        WEBP => "WebP",
        _ => "image",
    }
}

/// GIF via the `image` crate. Decodes **every** frame — each already composited to a full RGBA8
/// canvas by the decoder (GIF disposal handled) — so an animated GIF can play; a single-frame GIF
/// comes back as an ordinary still image (`animation: None`). GIF is 8-bit and carries no ICC, so
/// this stays on the simple RGBA8 path. Decode speed is not critical here (GIF is a rare fallback
/// format), and decoding all frames up front keeps the viewer/renderer trivial (a texture swap per
/// frame). Frame 0's pixels are duplicated into `DecodedImage::pixels` so the still-image code paths
/// (first paint, downscale, alpha scan) work unchanged.
fn decode_gif(bytes: &[u8]) -> Result<DecodedImage, DecodeError> {
    use image::codecs::gif::GifDecoder;
    use image::{AnimationDecoder, ImageDecoder};

    let decoder =
        GifDecoder::new(Cursor::new(bytes)).map_err(|e| DecodeError::Malformed(e.to_string()))?;

    // Two guards, because this backend has two multiplicands. `GifDecoder` is constructed directly
    // (no `ImageReader`, so no default memory limits) and every frame below is decoded to a *full*
    // RGBA canvas: the cost is `frames × w × h × 4`. Bounding the dimensions alone would leave the
    // frame count free to blow past RAM, and bounding the axes alone bounds nothing at all here —
    // GIF's dimensions are `u16`, so even the maximum 65535×65535 is under every per-axis cap while
    // asking for 17 GiB a frame. Hence: one byte-budget check on the canvas, one on the sequence.
    let (w, h) = decoder.dimensions();
    check_dims(w as usize, h as usize, 4, "GIF")?;
    let frame_bytes = (w as usize).saturating_mul(h as usize).saturating_mul(4).max(1);
    let max_frames = (MAX_ANIMATION_BYTES / frame_bytes).clamp(1, MAX_ANIMATION_FRAMES);

    // Collected one at a time rather than through `collect_frames`, so the budget can stop the walk
    // rather than discover it too late. Frames past the budget are dropped rather than raising an
    // error — a truncated animation still shows, and no real encoder gets anywhere near the cap.
    let mut frames = Vec::new();
    for frame in decoder.into_frames() {
        frames.push(frame.map_err(|e| DecodeError::Malformed(e.to_string()))?);
        if frames.len() >= max_frames {
            break;
        }
    }

    let first = frames
        .first()
        .ok_or_else(|| DecodeError::Malformed("GIF has no frames".into()))?;
    let (width, height) = first.buffer().dimensions();

    // Frame 0 pixels for the still path (and the first thing painted).
    let pixels = first.buffer().as_raw().clone();

    // A single-frame GIF is just a still image — skip the animation machinery entirely.
    let animation = (frames.len() > 1).then(|| Animation {
        frames: frames
            .into_iter()
            .map(|f| {
                let (num, den) = f.delay().numer_denom_ms();
                let raw_ms = num.checked_div(den).unwrap_or(0);
                // Browser-compatible clamp: GIFs commonly encode 0 (and sometimes 10 ms) meaning
                // "as fast as possible", which renderers treat as 100 ms. Anything ≥ 20 ms is
                // honored as authored.
                let delay_ms = if raw_ms < 20 { 100 } else { raw_ms };
                AnimationFrame { pixels: f.into_buffer().into_raw(), delay_ms }
            })
            .collect(),
    });

    Ok(DecodedImage {
        pixels,
        width,
        height,
        format: PixelFormat::Rgba8Unorm,
        bit_depth: 8,
        // GIF is palette-indexed with an optional transparent index → report RGBA (frames can
        // carry transparency); the opaque-alpha scan in `decode` flags the fully-opaque case.
        channels: 4,
        icc: None,
        source_format: "GIF",
        alpha_opaque: false, // set by `decode` after the final buffer is built
        downscaled_from: None,
        animation,
    })
}

/// Fallback for the formats zune has no decoder for: TIFF/TGA/ICO via the `image` crate (GIF
/// has its own multi-frame path, see [`decode_gif`]). Extracts the embedded ICC profile (where
/// the format carries one) and keeps float sources as 32-bit float RGBA (HDR). Decode speed here
/// is not critical (rare formats).
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
        alpha_opaque: false, // set by `decode` after the final buffer is built
        downscaled_from: None,
        animation: None,
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
                alpha_opaque: false,
                downscaled_from: None,
                animation: None,
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

    /// A lossless RGBA PNG must come back byte-for-byte (via the `image`-crate PNG path).
    #[test]
    fn png_rgba_roundtrip() {
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

    /// A 32-bit RGBA PNG whose alpha is uniformly opaque (e.g. a Windows screenshot) still reports
    /// its true RGBA channel count — the status bar and alpha-channel inspection stay intact — but
    /// is flagged `alpha_opaque` so the viewer doesn't default to the checker backdrop. A single
    /// transparent sample clears the flag. Regression: opaque RGBA screenshots showed transparency.
    #[test]
    fn opaque_rgba_png_flags_alpha_opaque_but_keeps_channels() {
        let mut src = image::RgbaImage::new(2, 1);
        src.put_pixel(0, 0, image::Rgba([200, 30, 40, 255]));
        src.put_pixel(1, 0, image::Rgba([10, 220, 60, 255]));
        let bytes = encode(&image::DynamicImage::ImageRgba8(src), image::ImageFormat::Png);

        let out = decode(&bytes, Some("png"), &DecodeOptions::default()).unwrap();
        assert_eq!(out.channels, 4, "an all-opaque RGBA PNG still reports its true RGBA format");
        assert!(out.alpha_opaque, "all-opaque alpha => no transparency to composite");
        assert_eq!(out.format, PixelFormat::Rgba8Unorm);
        assert_eq!(&out.pixels[0..4], &[200, 30, 40, 255]);

        // A single transparent pixel makes it genuinely transparent: flag clears.
        let mut src = image::RgbaImage::new(2, 1);
        src.put_pixel(0, 0, image::Rgba([200, 30, 40, 255]));
        src.put_pixel(1, 0, image::Rgba([10, 220, 60, 254]));
        let bytes = encode(&image::DynamicImage::ImageRgba8(src), image::ImageFormat::Png);
        let out = decode(&bytes, Some("png"), &DecodeOptions::default()).unwrap();
        assert_eq!(out.channels, 4);
        assert!(!out.alpha_opaque, "a single non-opaque sample is real transparency");
    }

    /// The opaque-alpha flag also covers 16-bit RGBA (alpha at full `0xffff`).
    #[test]
    fn opaque_rgba16_png_flags_alpha_opaque() {
        let mut src = image::ImageBuffer::<image::Rgba<u16>, _>::new(1, 1);
        src.put_pixel(0, 0, image::Rgba([0xFFFF, 0x8000, 0x0001, 0xFFFF]));
        let bytes = encode(&image::DynamicImage::ImageRgba16(src), image::ImageFormat::Png);

        let out = decode(&bytes, Some("png"), &DecodeOptions::default()).unwrap();
        assert_eq!(out.format, PixelFormat::Rgba16Unorm);
        assert_eq!(out.channels, 4, "an all-opaque 16-bit RGBA PNG still reports RGBA");
        assert!(out.alpha_opaque, "all-opaque 16-bit alpha => flagged");
    }

    /// A grayscale PNG decodes to Luma then expands to RGBA: gray replicated, opaque alpha.
    /// The reported source channel count stays 1 (for the status bar).
    #[test]
    fn grayscale_png_expands_to_rgba() {
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
    fn png16_stays_16bit() {
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

    /// A PNG's embedded ICC profile is surfaced by the `image`-crate PNG path (zune-png
    /// surfaced it via its metadata chunk; the routing change must not lose it).
    #[test]
    fn png_icc_profile_is_surfaced() {
        use image::ImageEncoder;

        let icc = lcms2::Profile::new_srgb().icc().unwrap();
        let mut buf = Vec::new();
        let mut enc = image::codecs::png::PngEncoder::new(&mut buf);
        enc.set_icc_profile(icc.clone()).expect("png encoder supports ICC");
        enc.write_image(&[200u8, 30, 40, 255], 1, 1, image::ExtendedColorType::Rgba8)
            .expect("encode fixture");

        // honor_icc=false so the raw profile bytes survive for the assertion.
        let opts = DecodeOptions { honor_icc: false, ..Default::default() };
        let out = decode(&buf, Some("png"), &opts).unwrap();
        assert_eq!(out.source_format, "PNG");
        assert_eq!(out.icc.as_deref(), Some(icc.as_slice()), "embedded ICC must be surfaced");
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

    /// Encode a GIF from a list of `(solid RGBA color, delay ms)` frames (test fixture). One frame
    /// → a still GIF; more → animated.
    fn encode_gif(w: u32, h: u32, frames: &[([u8; 4], u32)]) -> Vec<u8> {
        use image::codecs::gif::{GifEncoder, Repeat};
        use image::{Delay, Frame, RgbaImage};
        let mut buf = Vec::new();
        {
            let mut enc = GifEncoder::new(&mut buf);
            enc.set_repeat(Repeat::Infinite).expect("set repeat");
            for (color, delay_ms) in frames {
                let img = RgbaImage::from_pixel(w, h, image::Rgba(*color));
                let frame = Frame::from_parts(img, 0, 0, Delay::from_numer_denom_ms(*delay_ms, 1));
                enc.encode_frame(frame).expect("encode gif frame");
            }
        }
        buf
    }

    /// An animated GIF decodes every frame, carrying each frame's delay, with frame 0 also in
    /// `pixels` (the still path). Routed by the `GIF8` magic to the multi-frame decoder.
    #[test]
    fn animated_gif_decodes_all_frames_with_delays() {
        let bytes = encode_gif(4, 4, &[([220, 30, 40, 255], 100), ([20, 60, 220, 255], 60)]);
        let out = decode(&bytes, Some("gif"), &DecodeOptions::default()).unwrap();
        assert_eq!(out.source_format, "GIF");
        assert_eq!((out.width, out.height), (4, 4));
        assert_eq!(out.format, PixelFormat::Rgba8Unorm);
        assert_eq!(out.channels, 4);

        let anim = out.animation.as_ref().expect("animated GIF carries an Animation");
        assert_eq!(anim.frames.len(), 2);
        // Delays round-trip (GIF stores centiseconds; both are multiples of 10 ms, ≥ 20 ms).
        assert_eq!(anim.frames[0].delay_ms, 100);
        assert_eq!(anim.frames[1].delay_ms, 60);
        // Frame 0's pixels are duplicated into `pixels` so the still-image path works unchanged.
        assert_eq!(out.pixels, anim.frames[0].pixels);
        // Solid colors survive GIF palette quantization: frame 0 red-ish, frame 1 blue-ish.
        let f0 = &anim.frames[0].pixels;
        assert!(f0[0] > 180 && f0[1] < 90 && f0[2] < 100, "frame0 {},{},{}", f0[0], f0[1], f0[2]);
        let f1 = &anim.frames[1].pixels;
        assert!(f1[2] > 180 && f1[0] < 90, "frame1 {},{},{}", f1[0], f1[1], f1[2]);
    }

    /// GIF delays of 0 (and other sub-20 ms values) are clamped to 100 ms, matching how browsers
    /// treat "as fast as possible" — so a 0-delay GIF plays at a sane speed instead of spinning.
    #[test]
    fn gif_zero_delay_clamped_to_100ms() {
        let bytes = encode_gif(2, 2, &[([1, 2, 3, 255], 0), ([9, 8, 7, 255], 0)]);
        let out = decode(&bytes, Some("gif"), &DecodeOptions::default()).unwrap();
        let anim = out.animation.as_ref().expect("animated");
        assert!(anim.frames.iter().all(|f| f.delay_ms == 100), "0-delay frames clamp to 100 ms");
    }

    /// A single-frame GIF is an ordinary still image — no `Animation`, so no playback timer.
    #[test]
    fn single_frame_gif_is_still() {
        let bytes = encode_gif(2, 2, &[([10, 200, 60, 255], 100)]);
        let out = decode(&bytes, Some("gif"), &DecodeOptions::default()).unwrap();
        assert_eq!(out.source_format, "GIF");
        assert_eq!((out.width, out.height), (2, 2));
        assert!(out.animation.is_none(), "a single-frame GIF is a still image");
    }

    /// Corrupt input must surface an error, never panic (FFI-free path, but the viewer
    /// relies on this being a clean `Err`).
    #[test]
    fn garbage_input_errors() {
        let bytes = b"\x89PNG\r\n\x1a\n garbage that is not a real png body";
        let r = decode(bytes, Some("png"), &DecodeOptions::default());
        assert!(r.is_err());
    }

    // --- The one extension table --------------------------------------------------------------

    /// Every raw format the decoder *routes* (`raw::EXT_LABELS`) must also be a format the app
    /// admits it can open. Miss one and the file decodes fine when opened directly but is
    /// invisible to the Open dialog and skipped by folder navigation.
    #[test]
    fn raw_extensions_are_all_listed() {
        for (ext, label) in raw::EXT_LABELS {
            assert!(
                SUPPORTED_EXTENSIONS.contains(ext),
                "raw.rs routes .{ext} ({label}) but SUPPORTED_EXTENSIONS omits it"
            );
        }
    }

    /// The table is a set, not a bag — a duplicate would be harmless but signals an edit collision.
    #[test]
    fn extension_table_has_no_duplicates() {
        let mut seen = std::collections::HashSet::new();
        for ext in SUPPORTED_EXTENSIONS {
            assert!(seen.insert(*ext), ".{ext} is listed twice");
            assert_eq!(*ext, ext.to_ascii_lowercase(), "extensions are stored lower-case");
        }
    }

    /// The installer is the one copy of this list that cannot `use` it: `installer/fire.iss` is an
    /// Inno Setup script, and its per-format `Capabilities\FileAssociations` entries are what put
    /// fire in Explorer's "Open with" and Default Apps. If the two disagree, an installed fire
    /// either claims a format it cannot decode or fails to offer one it can — so read the script
    /// and compare the sets outright. Extensions live in lines of the form:
    ///
    /// ```text
    /// ...Capabilities\FileAssociations"; ValueType: string; ValueName: ".png"; ValueData: "Fire.png"...
    /// ```
    #[test]
    fn installer_associations_match_the_extension_table() {
        const ISS: &str = include_str!("../../../installer/fire.iss");

        let mut registered: Vec<String> = Vec::new();
        for line in ISS.lines() {
            if !line.contains(r"Capabilities\FileAssociations") {
                continue;
            }
            // Pull the `ValueName: ".ext"` field out of the line.
            let Some(rest) = line.split(r#"ValueName: "."#).nth(1) else {
                continue;
            };
            let Some(ext) = rest.split('"').next() else {
                continue;
            };
            registered.push(ext.to_ascii_lowercase());
        }
        assert!(
            !registered.is_empty(),
            "parsed no associations out of fire.iss — the script's format changed, and this test \
             is now silently vacuous"
        );

        let installer: std::collections::BTreeSet<&str> =
            registered.iter().map(|s| s.as_str()).collect();
        let decoder: std::collections::BTreeSet<&str> =
            SUPPORTED_EXTENSIONS.iter().copied().collect();

        let missing: Vec<_> = decoder.difference(&installer).collect();
        let extra: Vec<_> = installer.difference(&decoder).collect();
        assert!(
            missing.is_empty() && extra.is_empty(),
            "installer/fire.iss and SUPPORTED_EXTENSIONS disagree.\n  \
             decodable but not associated: {missing:?}\n  \
             associated but not decodable: {extra:?}"
        );
    }

    // --- Decode-bomb guards -------------------------------------------------------------------
    //
    // Each of these is a *tiny* file whose header declares an enormous image. They must come back
    // as a clean `Err` from the header check, never reaching an allocation: a `Vec` that fails to
    // allocate aborts the process (`handle_alloc_error`), which no `catch_unwind` can intercept —
    // so "the test passes" and "the test process is still alive" are the same assertion here.

    /// The product, not the axes, is what gets allocated: both of these pass a per-axis cap of
    /// 131072 and still ask for far more than [`MAX_DECODE_BYTES`].
    #[test]
    fn check_dims_bounds_the_product_not_just_each_axis() {
        // Comfortably inside the per-axis cap; 65535² × 4 ≈ 17 GiB.
        assert!(check_dims(65535, 65535, 4, "GIF").is_err());
        // A single oversized axis is still refused.
        assert!(check_dims(MAX_DECODE_DIM + 1, 1, 4, "PNG").is_err());
        // Overflowing the multiply is a rejection, not a wrap.
        assert!(check_dims(usize::MAX, usize::MAX, 16, "OpenEXR").is_err());
        // A large-but-real image still decodes: a 216-MP 16-bit scan is ~1.7 GiB.
        assert!(check_dims(18000, 12000, 8, "PNG").is_ok());
    }

    /// A 33-byte PNG whose IHDR claims 2³¹-ish pixels per side. The contract under test is
    /// behavioral, not which layer enforces it: a decode bomb comes back as a clean `Err` and the
    /// process survives. (Here the `png` crate's own memory limit happens to refuse it first;
    /// [`check_dims`] is the backstop for the sizes that slip under that.)
    #[test]
    fn png_decode_bomb_header_is_rejected() {
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(b"IHDR");
        ihdr.extend_from_slice(&0x7fff_ffffu32.to_be_bytes()); // width
        ihdr.extend_from_slice(&0x7fff_ffffu32.to_be_bytes()); // height
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit RGBA, deflate, no filter/interlace

        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"\x89PNG\r\n\x1a\n");
        bytes.extend_from_slice(&(ihdr.len() as u32 - 4).to_be_bytes());
        bytes.extend_from_slice(&ihdr);
        bytes.extend_from_slice(&crc32(&ihdr).to_be_bytes());

        assert!(
            decode(&bytes, Some("png"), &DecodeOptions::default()).is_err(),
            "a 2³¹×2³¹ IHDR must be refused, not allocated"
        );
    }

    /// Radiance stores its dimensions as decimal text, so a 52-byte file can claim a gigapixel
    /// canvas — 16 bytes/px once expanded to float RGBA, i.e. ~160 GiB.
    #[test]
    fn hdr_decode_bomb_header_is_rejected() {
        let bytes = b"#?RADIANCE\nFORMAT=32-bit_rle_rgbe\n\n-Y 99999 +X 99999\n";
        let err = decode(bytes, Some("hdr"), &DecodeOptions::default())
            .expect_err("a 99999² Radiance header must be refused");
        assert!(err.to_string().contains("decode guard"), "{err}");
    }

    /// GIF is the case a per-axis cap *cannot* catch, and the reason [`check_dims`] takes a byte
    /// budget: `u16` dimensions max out at 65535, comfortably under any sane axis guard, yet
    /// 65535² × 4 bytes is ~17 GiB — per frame. A complete but tiny file (one 1×1 frame on a
    /// 65535² logical screen) is all it takes; the canvas, not the frame, is what gets allocated.
    #[test]
    fn gif_max_u16_canvas_is_rejected_by_the_byte_budget() {
        #[rustfmt::skip]
        let bytes: Vec<u8> = [
            b"GIF89a".as_slice(),
            &[0xff, 0xff],              // logical screen width  = 65535
            &[0xff, 0xff],              // logical screen height = 65535
            &[0x80, 0x00, 0x00],        // global color table (2 entries), bg index, aspect
            &[0x00, 0x00, 0x00],        // GCT[0] = black
            &[0xff, 0xff, 0xff],        // GCT[1] = white
            &[0x2c],                    // image separator
            &[0x00, 0x00, 0x00, 0x00],  // frame left, top
            &[0x01, 0x00, 0x01, 0x00],  // frame width = 1, height = 1
            &[0x00],                    // no local color table
            &[0x02],                    // LZW minimum code size
            &[0x02, 0x44, 0x01],        // one sub-block: CLEAR, index 0, EOI
            &[0x00],                    // block terminator
            &[0x3b],                    // trailer
        ]
        .concat();

        let err = decode(&bytes, Some("gif"), &DecodeOptions::default())
            .expect_err("a 65535² GIF canvas must be refused");
        assert!(err.to_string().contains("decode guard"), "{err}");
    }

    /// An animated GIF's cost is `frames × w × h × 4`, so the frame count is bounded too — a small
    /// canvas must not let an unbounded sequence through. 100 frames of 4×4 is far under the cap
    /// and decodes whole; the cap itself is exercised by [`MAX_ANIMATION_FRAMES`] arithmetic.
    #[test]
    fn animation_frame_budget_admits_real_sequences() {
        let frames: Vec<_> = (0..100u32)
            .map(|i| ([(i * 2) as u8, 40, 200, 255], 40u32))
            .collect();
        let bytes = encode_gif(4, 4, &frames);
        let out = decode(&bytes, Some("gif"), &DecodeOptions::default()).unwrap();
        let anim = out.animation.as_ref().expect("animated");
        assert_eq!(anim.frames.len(), 100, "a 100-frame 4×4 GIF is nowhere near the budget");

        // The budget divides the byte cap by the canvas size, and is clamped to the frame ceiling.
        let tiny_canvas_budget = (MAX_ANIMATION_BYTES / (4 * 4 * 4)).min(MAX_ANIMATION_FRAMES);
        assert_eq!(tiny_canvas_budget, MAX_ANIMATION_FRAMES);
    }

    /// CRC-32 (IEEE) for building the PNG fixture above.
    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xffff_ffffu32;
        for &b in data {
            crc ^= b as u32;
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xedb8_8320 & mask);
            }
        }
        !crc
    }
}
