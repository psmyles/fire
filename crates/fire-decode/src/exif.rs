//! EXIF display **orientation**: locating the tag in a container, and applying it to pixels.
//!
//! A camera writes the sensor readout unrotated and records how to turn it upright in one EXIF
//! tag (`Orientation`, 0x0112). None of the decode backends honor it — zune, the `image` crate,
//! the `tiff` crate and `exr` all hand back the pixels as stored — so a portrait phone photo
//! displays on its side. [`crate::decode`] closes that gap by reading the tag straight out of the
//! source bytes and rotating the decoded buffer *once*, before the ICC transform and the
//! downscale-to-fit, so everything downstream (the status bar's dimensions, the flipbook's grid
//! math, `downscaled_from`, the pixel inspector) sees the upright axes and no per-frame work is
//! added to the renderer.
//!
//! ## Where the tag lives
//!
//! The tag always sits inside a small TIFF stream; only the wrapper differs, so each container
//! here just has to *find* that stream and hand it to [`orientation_from_tiff`]:
//!
//!   - **TIFF** (including the TIFF-structured raws): the file *is* the TIFF stream.
//!   - **JPEG**: an `APP1` segment whose payload begins `Exif\0\0`.
//!   - **PNG**: the `eXIf` chunk (PNG third edition) — the bare TIFF stream.
//!   - **WebP**: the RIFF `EXIF` chunk. Some encoders write the JPEG-style `Exif\0\0` prefix into
//!     it even though the spec says bare TIFF, so both are accepted (also for `eXIf`).
//!
//! **HEIC/AVIF are deliberately absent.** ISOBMFF describes rotation with the `irot`/`imir` item
//! properties, which libheif applies during the decode itself; such files almost always *also*
//! carry an agreeing EXIF tag, so honoring it here would rotate an already-upright image a second
//! time. PSD, EXR and GIF have no display orientation to honor.
//!
//! Every read is bounds-checked (they return `Option` rather than indexing), so a truncated or
//! hostile file yields "upright", never a panic.

use crate::DecodedImage;

/// The EXIF `Orientation` (1..=8) for a source file's bytes. Falls back to `1` — upright, a no-op
/// for [`apply`] — when the container carries no tag, isn't one the tag is read from, or is
/// malformed. Callers do not need to special-case that: `apply(img, orientation(bytes))` is
/// correct for every input.
pub fn orientation(bytes: &[u8]) -> u16 {
    tiff_stream(bytes)
        .and_then(orientation_from_tiff)
        .filter(|o| (1..=8).contains(o))
        .unwrap_or(1)
}

/// Apply an EXIF `orientation` (2..=8) to a decoded image in place, rotating/flipping the pixels
/// so the image displays upright. Orientation 1 (or any out-of-range value) is a no-op and costs
/// nothing — the overwhelmingly common case, so an image with no tag is never copied. Works at
/// `bytes_per_pixel` granularity, so it is format-agnostic (8/16-bit and float alike).
///
/// The 90°/270° cases swap `width`/`height`, which is why this runs before the downscale and
/// before anything reads the dimensions.
pub fn apply(img: &mut DecodedImage, orientation: u16) {
    if !(2..=8).contains(&orientation) {
        return;
    }
    let bpp = img.format.bytes_per_pixel();
    let (w, h) = (img.width as usize, img.height as usize);
    let needed = w * h * bpp;
    if img.pixels.len() < needed {
        return;
    }
    // Orientations 5..=8 are 90°/270° rotations (and the diagonal mirrors), which swap axes.
    let (ow, oh) = if (5..=8).contains(&orientation) {
        (h, w)
    } else {
        (w, h)
    };

    // Every buffer — the canvas and each animation frame — gets the same rotation; the
    // dimension swap happens once, afterwards.
    img.transform_buffers(needed, |pixels| {
        let mut out = vec![0u8; ow * oh * bpp];
        for sy in 0..h {
            for sx in 0..w {
                let (dx, dy) = match orientation {
                    2 => (w - 1 - sx, sy),         // mirror horizontal
                    3 => (w - 1 - sx, h - 1 - sy), // rotate 180
                    4 => (sx, h - 1 - sy),         // mirror vertical
                    5 => (sy, sx),                 // transpose (mirror along main diagonal)
                    6 => (h - 1 - sy, sx),         // rotate 90° CW
                    7 => (h - 1 - sy, w - 1 - sx), // transverse (mirror along anti-diagonal)
                    8 => (sy, w - 1 - sx),         // rotate 90° CCW
                    _ => (sx, sy),
                };
                let si = (sy * w + sx) * bpp;
                let di = (dy * ow + dx) * bpp;
                out[di..di + bpp].copy_from_slice(&pixels[si..si + bpp]);
            }
        }
        *pixels = out;
    });
    img.width = ow as u32;
    img.height = oh as u32;
}

// --- container unwrapping -----------------------------------------------------

/// The embedded TIFF stream the `Orientation` tag would live in, by container magic. `None` for a
/// container we don't read the tag from (see the module header on HEIC/AVIF) or one that carries
/// no EXIF at all.
fn tiff_stream(b: &[u8]) -> Option<&[u8]> {
    if b.starts_with(b"II\x2a\x00") || b.starts_with(b"MM\x00\x2a") {
        Some(b) // plain TIFF, and the TIFF-structured raws
    } else if b.starts_with(&[0xFF, 0xD8]) {
        jpeg_exif(b)
    } else if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        png_exif(b)
    } else if b.len() >= 12 && b.starts_with(b"RIFF") && &b[8..12] == b"WEBP" {
        webp_exif(b)
    } else {
        None
    }
}

/// Strip the JPEG-style `Exif\0\0` header if it is there. PNG's `eXIf` and WebP's `EXIF` chunks
/// are defined as the bare TIFF stream, but encoders that reuse their JPEG writer emit the prefix
/// anyway; accepting both costs six bytes of tolerance.
fn strip_exif_header(data: &[u8]) -> &[u8] {
    data.strip_prefix(b"Exif\0\0").unwrap_or(data)
}

/// The TIFF stream inside a JPEG's `APP1` segment. Walks the marker segments only — never the
/// entropy-coded scan data — and gives up at `SOS`, since EXIF is always in the header.
fn jpeg_exif(b: &[u8]) -> Option<&[u8]> {
    let mut i = 2;
    while i < b.len() {
        if b[i] != 0xFF {
            return None; // desynchronized: not sitting on a marker
        }
        // A marker may be padded with any number of leading 0xFF fill bytes.
        let mut m = i + 1;
        while b.get(m) == Some(&0xFF) {
            m += 1;
        }
        let marker = *b.get(m)?;
        match marker {
            0xDA | 0xD9 => return None, // SOS / EOI: the header is over
            // TEM, RSTn and a stray SOI carry no length field.
            0x01 | 0xD0..=0xD8 => {
                i = m + 1;
                continue;
            }
            _ => {}
        }
        // The length field counts itself, so the payload is `len - 2` bytes.
        let len = u16::from_be_bytes([*b.get(m + 1)?, *b.get(m + 2)?]) as usize;
        if len < 2 {
            return None;
        }
        let payload = b.get(m + 3..m + 1 + len)?;
        if marker == 0xE1 {
            // An APP1 that isn't EXIF (XMP is the common one) just falls through to the next
            // segment — editors write both, in either order.
            if let Some(tiff) = payload.strip_prefix(b"Exif\0\0") {
                return Some(tiff);
            }
        }
        i = m + 1 + len;
    }
    None
}

/// The TIFF stream in a PNG's `eXIf` chunk. Chunk *data* is skipped by its declared length, so
/// walking past a multi-megabyte `IDAT` costs nothing; the chunk may legally sit either side of
/// the image data, so the walk runs to `IEND`.
fn png_exif(b: &[u8]) -> Option<&[u8]> {
    let mut i = 8usize; // past the signature
    loop {
        let len = u32::from_be_bytes(b.get(i..i.checked_add(4)?)?.try_into().ok()?) as usize;
        let kind = b.get(i + 4..i + 8)?;
        if kind == b"IEND" {
            return None;
        }
        if kind == b"eXIf" {
            return Some(strip_exif_header(b.get(i + 8..i + 8 + len)?));
        }
        // length (4) + type (4) + data + CRC (4)
        i = i.checked_add(12)?.checked_add(len)?;
    }
}

/// The TIFF stream in a WebP's RIFF `EXIF` chunk (extended WebP only — a simple lossy/lossless
/// file has no chunk list past `VP8 `/`VP8L`, and the walk simply runs off the end).
fn webp_exif(b: &[u8]) -> Option<&[u8]> {
    let mut i = 12usize; // past "RIFF" + size + "WEBP"
    loop {
        let kind = b.get(i..i.checked_add(4)?)?;
        let len = u32::from_le_bytes(b.get(i + 4..i + 8)?.try_into().ok()?) as usize;
        if kind == b"EXIF" {
            return Some(strip_exif_header(b.get(i + 8..i + 8 + len)?));
        }
        // RIFF pads every chunk to an even size, and the pad byte is not counted in `len`.
        i = i.checked_add(8)?.checked_add(len)?.checked_add(len & 1)?;
    }
}

// --- TIFF directory read ------------------------------------------------------

/// Read `Orientation` (0x0112) out of a TIFF stream's **IFD0**.
///
/// Only IFD0, on purpose: it describes the main image, whereas the thumbnail IFD (IFD0's `next`
/// link) and the EXIF sub-IFD can carry a different value. Honoring one of those would rotate the
/// main image to suit a thumbnail — the same trap [`crate::raw`]'s wider walk guards against with
/// its first-one-wins rule.
fn orientation_from_tiff(t: &[u8]) -> Option<u16> {
    let le = if t.starts_with(b"II") {
        true
    } else if t.starts_with(b"MM") {
        false
    } else {
        return None;
    };
    if rd_u16(t, 2, le)? != 42 {
        return None;
    }
    let base = rd_u32(t, 4, le)? as usize;
    let count = rd_u16(t, base, le)? as usize;
    if count > 4096 {
        return None; // implausible entry count — treat as malformed
    }
    for e in 0..count {
        let eo = base + 2 + e * 12;
        if rd_u16(t, eo, le)? == 0x0112 {
            let typ = rd_u16(t, eo + 2, le)?;
            return entry_scalar(t, eo, typ, le).map(|v| v as u16);
        }
    }
    None
}

/// Read a single-value IFD entry (`count == 1`) as a u32, honoring BYTE/SHORT/LONG types.
/// The value sits inline in the entry's 4-byte value field (all these types fit).
pub(crate) fn entry_scalar(b: &[u8], eo: usize, typ: u16, le: bool) -> Option<u32> {
    match typ {
        1 => rd_u8(b, eo + 8).map(|v| v as u32),      // BYTE
        3 => rd_u16(b, eo + 8, le).map(|v| v as u32), // SHORT
        _ => rd_u32(b, eo + 8, le),                   // LONG (4) and best-effort fallback
    }
}

// --- bounds-checked primitive reads ------------------------------------------

pub(crate) fn rd_u8(b: &[u8], o: usize) -> Option<u8> {
    b.get(o).copied()
}

pub(crate) fn rd_u16(b: &[u8], o: usize, le: bool) -> Option<u16> {
    let s = b.get(o..o + 2)?;
    Some(if le {
        u16::from_le_bytes([s[0], s[1]])
    } else {
        u16::from_be_bytes([s[0], s[1]])
    })
}

pub(crate) fn rd_u32(b: &[u8], o: usize, le: bool) -> Option<u32> {
    let s = b.get(o..o + 4)?;
    Some(if le {
        u32::from_le_bytes([s[0], s[1], s[2], s[3]])
    } else {
        u32::from_be_bytes([s[0], s[1], s[2], s[3]])
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PixelFormat;

    /// A minimal little-endian TIFF stream whose IFD0 holds just `Orientation`.
    fn tiff_stream_with(orientation: u16) -> Vec<u8> {
        let mut t = Vec::new();
        t.extend_from_slice(b"II\x2a\x00");
        t.extend_from_slice(&8u32.to_le_bytes()); // IFD0 at offset 8
        t.extend_from_slice(&1u16.to_le_bytes()); // one entry
        t.extend_from_slice(&0x0112u16.to_le_bytes()); // Orientation
        t.extend_from_slice(&3u16.to_le_bytes()); // SHORT
        t.extend_from_slice(&1u32.to_le_bytes()); // count
        t.extend_from_slice(&(orientation as u32).to_le_bytes()); // inline value
        t.extend_from_slice(&0u32.to_le_bytes()); // no next IFD
        t
    }

    /// A JPEG carrying `orientation` in `APP1`: the real encoder's output with an EXIF segment
    /// spliced in right after the SOI, which is where a camera puts it.
    fn jpeg_with(orientation: u16) -> Vec<u8> {
        let src = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            4,
            2,
            image::Rgb([90, 120, 200]),
        ));
        let mut buf = std::io::Cursor::new(Vec::new());
        src.write_to(&mut buf, image::ImageFormat::Jpeg).unwrap();
        let plain = buf.into_inner();

        let tiff = tiff_stream_with(orientation);
        let mut app1 = b"Exif\0\0".to_vec();
        app1.extend_from_slice(&tiff);

        let mut out = vec![0xFF, 0xD8, 0xFF, 0xE1];
        out.extend_from_slice(&((app1.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(&app1);
        out.extend_from_slice(&plain[2..]); // the encoder's stream, minus its own SOI
        out
    }

    /// A PNG chunk list after the signature: `length` (big-endian), `type`, data, CRC. The CRC is
    /// written as zeroes — nothing on this path verifies it.
    fn png_chunks(items: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
        let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
        for (kind, data) in items {
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            out.extend_from_slice(*kind);
            out.extend_from_slice(data);
            out.extend_from_slice(&[0; 4]);
        }
        out
    }

    /// A RIFF/WebP chunk list: `type`, `size` (little-endian), data, and a pad byte when the size
    /// is odd. The opposite field order and endianness from PNG, which is the point of testing it.
    fn webp_chunks(items: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
        let mut out = b"RIFF\0\0\0\0WEBP".to_vec();
        for (kind, data) in items {
            out.extend_from_slice(*kind);
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(data);
            if data.len() % 2 == 1 {
                out.push(0);
            }
        }
        out
    }

    fn img(w: u32, h: u32, pixels: Vec<u8>) -> DecodedImage {
        DecodedImage {
            pixels,
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

    #[test]
    fn reads_orientation_from_a_bare_tiff() {
        assert_eq!(orientation(&tiff_stream_with(6)), 6);
        // Big-endian ("MM") is the other half of TIFF, and the byte order applies to every field.
        let mut be = Vec::new();
        be.extend_from_slice(b"MM\x00\x2a");
        be.extend_from_slice(&8u32.to_be_bytes());
        be.extend_from_slice(&1u16.to_be_bytes());
        be.extend_from_slice(&0x0112u16.to_be_bytes());
        be.extend_from_slice(&3u16.to_be_bytes());
        be.extend_from_slice(&1u32.to_be_bytes());
        be.extend_from_slice(&(8u32 << 16).to_be_bytes()); // SHORT 8, big-endian in a LONG slot
        be.extend_from_slice(&0u32.to_be_bytes());
        assert_eq!(orientation(&be), 8);
    }

    #[test]
    fn reads_orientation_from_a_jpeg_app1_segment() {
        for o in 1..=8u16 {
            assert_eq!(orientation(&jpeg_with(o)), o, "APP1 orientation {o}");
        }
    }

    /// An `APP1` that is *not* EXIF (XMP is the usual one) must not stop the walk — cameras and
    /// editors routinely write both, in either order.
    #[test]
    fn skips_a_non_exif_app1_before_the_exif_one() {
        let base = jpeg_with(6);
        let xmp = b"http://ns.adobe.com/xap/1.0/\0<x:xmpmeta/>";
        let mut out = vec![0xFF, 0xD8, 0xFF, 0xE1];
        out.extend_from_slice(&((xmp.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(xmp);
        out.extend_from_slice(&base[2..]);
        assert_eq!(orientation(&out), 6);
    }

    #[test]
    fn reads_orientation_from_a_png_exif_chunk() {
        let png = png_chunks(&[
            (b"IHDR", vec![0; 13]),
            // A big `IDAT` is skipped by its length, not searched.
            (b"IDAT", vec![0x42; 4096]),
            (b"eXIf", tiff_stream_with(3)),
            (b"IEND", Vec::new()),
        ]);
        assert_eq!(orientation(&png), 3);

        // No `eXIf` at all: upright.
        let bare = png_chunks(&[(b"IHDR", vec![0; 13]), (b"IEND", Vec::new())]);
        assert_eq!(orientation(&bare), 1);
    }

    /// WebP's chunk sizes are little-endian and odd-sized chunks are padded — get either wrong and
    /// the walk lands mid-chunk and finds nothing. The `Exif\0\0` prefix some encoders add is
    /// tolerated here too.
    #[test]
    fn reads_orientation_from_a_webp_exif_chunk() {
        let mut prefixed = b"Exif\0\0".to_vec();
        prefixed.extend_from_slice(&tiff_stream_with(7));
        let webp = webp_chunks(&[
            (b"VP8X", vec![0; 10]),
            (b"ICCP", vec![0x11; 15]), // odd length: exercises the pad byte
            (b"EXIF", prefixed),
        ]);
        assert_eq!(orientation(&webp), 7);
    }

    /// Anything we don't read the tag from — and anything malformed — reports upright rather than
    /// failing or, worse, rotating on garbage. HEIC is in this list deliberately: libheif has
    /// already applied `irot`/`imir` by the time the pixels reach us (see the module header).
    #[test]
    fn unreadable_and_unsupported_containers_report_upright() {
        assert_eq!(orientation(b""), 1);
        assert_eq!(orientation(b"not an image at all"), 1);
        assert_eq!(orientation(b"\0\0\0\x18ftypheic"), 1);
        assert_eq!(orientation(b"GIF89a"), 1);
        // Truncated at every length: nothing may panic, and a segment cut short must not have a
        // rotation read out of bytes that aren't there. `jpeg_with` puts EXIF first, so the tag is
        // legitimately complete once SOI + the APP1 header + `Exif\0\0` + the TIFF stream are in.
        let full = jpeg_with(6);
        let complete = 2 + 2 + 2 + 6 + tiff_stream_with(6).len();
        for n in 0..full.len() {
            let want = if n < complete { 1 } else { 6 };
            assert_eq!(orientation(&full[..n]), want, "truncated to {n} bytes");
        }
        // An out-of-range tag value is not a rotation we can make sense of.
        assert_eq!(orientation(&tiff_stream_with(9)), 1);
        assert_eq!(orientation(&tiff_stream_with(0)), 1);
    }

    #[test]
    fn rotate_90_cw_swaps_axes() {
        // 2 wide, 1 tall: A B
        let mut im = img(2, 1, vec![1, 0, 0, 255, 2, 0, 0, 255]);
        apply(&mut im, 6); // rotate 90° CW -> 1 wide, 2 tall, A on top
        assert_eq!((im.width, im.height), (1, 2));
        assert_eq!(im.pixels, vec![1, 0, 0, 255, 2, 0, 0, 255]);
    }

    #[test]
    fn mirror_horizontal() {
        let mut im = img(2, 1, vec![1, 0, 0, 255, 2, 0, 0, 255]);
        apply(&mut im, 2); // mirror horizontal -> B then A
        assert_eq!((im.width, im.height), (2, 1));
        assert_eq!(im.pixels, vec![2, 0, 0, 255, 1, 0, 0, 255]);
    }

    #[test]
    fn identity_and_out_of_range_are_noops() {
        for o in [0u16, 1, 9, 65535] {
            let mut im = img(2, 1, vec![1, 0, 0, 255, 2, 0, 0, 255]);
            let before = im.pixels.clone();
            apply(&mut im, o);
            assert_eq!(im.pixels, before, "orientation {o} must not touch pixels");
            assert_eq!((im.width, im.height), (2, 1));
        }
    }

    /// A buffer shorter than `width * height * bpp` (a truncated decode that still reported full
    /// dimensions) must be left alone rather than indexed past its end.
    #[test]
    fn short_buffer_is_left_alone() {
        let mut im = img(4, 4, vec![0; 8]);
        apply(&mut im, 6);
        assert_eq!((im.width, im.height), (4, 4));
        assert_eq!(im.pixels.len(), 8);
    }

    /// Orientation is applied at `bytes_per_pixel` granularity, so a float (HDR) buffer rotates
    /// exactly like an 8-bit one — a 32-bit TIFF with an orientation tag is a real thing.
    #[test]
    fn float_pixels_rotate_by_whole_samples() {
        let px: Vec<u8> = [1.0f32, 0.0, 0.0, 1.0, 2.0, 0.0, 0.0, 1.0]
            .iter()
            .flat_map(|f| f.to_ne_bytes())
            .collect();
        let mut im = DecodedImage {
            format: PixelFormat::Rgba32Float,
            ..img(2, 1, px.clone())
        };
        apply(&mut im, 3); // rotate 180 on a 2x1 == swap the two pixels
        assert_eq!((im.width, im.height), (2, 1));
        assert_eq!(&im.pixels[..16], &px[16..]);
        assert_eq!(&im.pixels[16..], &px[..16]);
    }
}
