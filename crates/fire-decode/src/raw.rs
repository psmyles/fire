//! Camera-raw support via **embedded-preview extraction** (plan A).
//!
//! A raw file (CR2/CR3, NEF, ARW, RAF, ORF, RW2, DNG, …) is not a single image format
//! but a per-vendor container wrapped around the sensor mosaic. Fully *developing* that
//! mosaic (demosaic + white balance + camera color matrices) is slow and is squarely at
//! odds with the project's time-to-first-pixel goal. But every consumer raw file also
//! embeds a full-size, camera-rendered **JPEG preview** — the image the camera's own LCD
//! shows. Extracting that preview is fast and gives a correct, white-balanced picture, so
//! that is what the viewer displays.
//!
//! This module's job is purely to *locate* the largest embedded JPEG and report the
//! file's display orientation; the actual JPEG decode is handed back to the normal zune
//! hot path in [`crate`], so ICC handling, bit-depth normalization, and downscale-to-fit
//! all come for free.
//!
//! ## How the preview is found
//!
//! Most raws are TIFF-structured (CR2, NEF, ARW, DNG, ORF, PEF, SRW, RW2, 3FR, IIQ, …),
//! so the primary path is a small, bounds-checked TIFF/EXIF **IFD walk** that collects
//! every JPEG preview a directory points at (`JPEGInterchangeFormat`, or single-strip
//! JPEG-compressed images) across IFD0's chain, its `SubIFDs`, and the EXIF IFD, and reads
//! the `Orientation` tag. Fujifilm RAF carries the preview offset/length in a fixed header
//! field. Anything else (Canon CR3 / ISOBMFF, Canon CRW / CIFF, …) — and any TIFF whose
//! IFD walk comes up empty — falls back to a **whole-file JPEG marker scan** that finds
//! `FF D8 FF` start-of-image runs. Every candidate is validated by probing its JPEG
//! Start-Of-Frame for real dimensions (no full decode), and the largest by pixel area wins.
//!
//! All parsing is pure Rust and bounds-checked (reads return `Option`, never index past the
//! buffer), so a malformed/truncated raw yields "no preview found", never a panic.

use crate::{DecodedImage, PixelFormat};

/// Map a lowercase raw file extension to a human-readable status-bar label. This is the
/// authoritative set of extensions the decoder treats as camera raw; the installer's
/// `assoc\raw` associations and `folder.rs`'s navigation list mirror the common subset.
const EXT_LABELS: &[(&str, &str)] = &[
    ("cr2", "Canon CR2"),
    ("cr3", "Canon CR3"),
    ("crw", "Canon CRW"),
    ("nef", "Nikon NEF"),
    ("nrw", "Nikon NRW"),
    ("arw", "Sony ARW"),
    ("srf", "Sony SRF"),
    ("sr2", "Sony SR2"),
    ("raf", "Fujifilm RAF"),
    ("orf", "Olympus ORF"),
    ("rw2", "Panasonic RW2"),
    ("pef", "Pentax PEF"),
    ("srw", "Samsung SRW"),
    ("dng", "Adobe DNG"),
    ("x3f", "Sigma X3F"),
    ("3fr", "Hasselblad 3FR"),
    ("fff", "Hasselblad FFF"),
    ("iiq", "Phase One IIQ"),
    ("erf", "Epson ERF"),
    ("mrw", "Minolta MRW"),
    ("dcr", "Kodak DCR"),
    ("kdc", "Kodak KDC"),
    ("mef", "Mamiya MEF"),
    ("mos", "Leaf MOS"),
    ("rwl", "Leica RWL"),
    ("gpr", "GoPro GPR"),
    ("raw", "Camera RAW"),
];

/// Identify a camera-raw stream and return its status-bar label, or `None` if `bytes`/`ext`
/// don't look like a raw the previewer handles.
///
/// Magic bytes are checked first (they identify a format even with a wrong/absent
/// extension), then the extension map — the reliable signal for the many TIFF-structured
/// raws that share TIFF's magic and can't be told apart by their header alone.
pub fn label(bytes: &[u8], ext: Option<&str>) -> Option<&'static str> {
    if let Some(l) = label_from_magic(bytes) {
        return Some(l);
    }
    let ext = ext?;
    let lower = ext.to_ascii_lowercase();
    EXT_LABELS
        .iter()
        .find(|(e, _)| *e == lower)
        .map(|(_, label)| *label)
}

/// Recognize the raws that carry an unambiguous signature, so a no-extension open (drag of a
/// renamed file, pipe-forward) still routes correctly. Deliberately does **not** match bare
/// TIFF — a plain `.tif` must not be mistaken for raw — only TIFF *plus* a raw marker.
fn label_from_magic(b: &[u8]) -> Option<&'static str> {
    // Canon CR2: a little-endian TIFF whose header carries "CR\x02" at offset 8.
    if b.len() >= 12 && b.starts_with(b"II") && &b[8..10] == b"CR" && b[10] == 2 {
        return Some("Canon CR2");
    }
    // Canon CR3: ISOBMFF with major brand "crx " in the ftyp box.
    if b.len() >= 12 && &b[4..8] == b"ftyp" && &b[8..12] == b"crx " {
        return Some("Canon CR3");
    }
    // Fujifilm RAF: ASCII header magic.
    if b.starts_with(b"FUJIFILMCCD-RAW") {
        return Some("Fujifilm RAF");
    }
    // Sigma X3F (Foveon).
    if b.starts_with(b"FOVb") {
        return Some("Sigma X3F");
    }
    None
}

/// A located embedded preview: the JPEG byte range and the file's display orientation.
pub struct Preview<'a> {
    /// The embedded JPEG stream (decoded by the caller through the normal JPEG path). May
    /// extend to end-of-file for marker-scan hits; JPEG decoders stop at the EOI marker.
    pub jpeg: &'a [u8],
    /// EXIF orientation (1..=8) to apply to the decoded preview; 1 when unknown.
    pub orientation: u16,
}

/// Find the largest embedded JPEG preview in a raw file, plus its display orientation.
///
/// Returns `None` when no usable preview is present (e.g. a `.raw` sensor dump with no
/// embedded JPEG, or a truncated file).
pub fn find_preview(b: &[u8]) -> Option<Preview<'_>> {
    let mut orientation = 1u16;
    let mut best = None;

    if b.starts_with(b"II") || b.starts_with(b"MM") {
        // TIFF-structured raw: walk the IFD tree for preview candidates + orientation.
        let (cands, o) = collect_tiff(b);
        orientation = o;
        best = pick_largest(b, &cands);
    } else if b.starts_with(b"FUJIFILMCCD-RAW") {
        if let Some(c) = raf_preview(b) {
            best = pick_largest(b, &[c]);
        }
    }

    // Fallback for non-TIFF containers (CR3/ISOBMFF, CRW/CIFF) and for any TIFF whose
    // directories didn't yield a decodable preview: scan the whole file for JPEG streams.
    if best.is_none() {
        best = pick_largest(b, &scan_jpeg_markers(b));
    }

    let (off, end) = best?;
    Some(Preview { jpeg: &b[off..end], orientation })
}

/// Apply an EXIF `orientation` (2..=8) to a decoded image in place, rotating/flipping the
/// pixels so the image displays upright. Orientation 1 (or any out-of-range value) is a
/// no-op. Works at `bytes_per_pixel` granularity, so it is format-agnostic.
///
/// We apply the orientation read from the raw's TIFF directory to the embedded preview: the
/// major brands (Canon/Nikon/Sony) store the full-size preview in sensor orientation and
/// describe the upright rotation with that one tag, so a portrait shot would otherwise show
/// sideways. (Containers we can't read an orientation from default to 1; their previews are
/// generally stored already upright.)
pub fn apply_orientation(img: &mut DecodedImage, orientation: u16) {
    if !(2..=8).contains(&orientation) {
        return;
    }
    let bpp = img.format.bytes_per_pixel();
    let (w, h) = (img.width as usize, img.height as usize);
    if img.pixels.len() < w * h * bpp {
        return;
    }
    // Orientations 5..=8 are 90°/270° rotations (and the diagonal mirrors), which swap axes.
    let (ow, oh) = if (5..=8).contains(&orientation) { (h, w) } else { (w, h) };
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
            out[di..di + bpp].copy_from_slice(&img.pixels[si..si + bpp]);
        }
    }

    img.pixels = out;
    img.width = ow as u32;
    img.height = oh as u32;
    // Preview is always 8-bit RGBA at this point, but keep the invariant explicit.
    debug_assert!(matches!(img.format, PixelFormat::Rgba8Unorm | PixelFormat::Rgba16Unorm));
}

// --- preview candidate selection ---------------------------------------------

/// From a list of `(offset, byte-len)` candidate ranges, return the `(start, end)` of the
/// one whose JPEG Start-Of-Frame reports the largest pixel area. Candidates that don't begin
/// with a JPEG SOI (`FF D8`) or whose dimensions can't be probed are skipped, so junk ranges
/// (e.g. a stray `FF D8 FF` inside sensor data) are filtered cheaply without a full decode.
fn pick_largest(b: &[u8], cands: &[(usize, usize)]) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize, u64)> = None;
    for &(off, len) in cands {
        let end = off.saturating_add(len).min(b.len());
        let slice = match b.get(off..end) {
            Some(s) => s,
            None => continue,
        };
        if slice.len() < 4 || slice[0] != 0xFF || slice[1] != 0xD8 {
            continue;
        }
        if let Some((w, h)) = jpeg_dimensions(slice) {
            let area = w as u64 * h as u64;
            if area == 0 {
                continue;
            }
            if best.is_none_or(|(_, _, a)| area > a) {
                best = Some((off, end, area));
            }
        }
    }
    best.map(|(o, e, _)| (o, e))
}

/// Probe a JPEG's pixel dimensions from its Start-Of-Frame marker without decoding it.
/// `b` must start at the SOI. Returns `(width, height)`.
fn jpeg_dimensions(b: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2; // skip the SOI (FF D8)
    while i + 4 <= b.len() {
        if b[i] != 0xFF {
            i += 1; // tolerate fill/padding bytes between segments
            continue;
        }
        let marker = b[i + 1];
        // Standalone markers (no length field): SOI/EOI, restart markers, TEM.
        if marker == 0xD8 || marker == 0xD9 || (0xD0..=0xD7).contains(&marker) || marker == 0x01 {
            i += 2;
            continue;
        }
        let len = u16::from_be_bytes([*b.get(i + 2)?, *b.get(i + 3)?]) as usize;
        if len < 2 {
            return None;
        }
        // Start-Of-Frame markers carry the dimensions: C0..=CF except DHT(C4), JPG(C8), DAC(CC).
        if (0xC0..=0xCF).contains(&marker) && marker != 0xC4 && marker != 0xC8 && marker != 0xCC {
            // segment: FF, marker, len(2), precision(1), height(2), width(2)
            let h = u16::from_be_bytes([*b.get(i + 5)?, *b.get(i + 6)?]) as u32;
            let w = u16::from_be_bytes([*b.get(i + 7)?, *b.get(i + 8)?]) as u32;
            return Some((w, h));
        }
        i += 2 + len;
    }
    None
}

/// Whole-file scan for embedded JPEG streams: every `FF D8 FF` (SOI + a marker) is a
/// candidate whose range runs to end-of-file (the JPEG decoder stops at EOI). Capped so a
/// pathological input can't produce an unbounded candidate list.
fn scan_jpeg_markers(b: &[u8]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 3 <= b.len() {
        if b[i] == 0xFF && b[i + 1] == 0xD8 && b[i + 2] == 0xFF {
            out.push((i, b.len() - i));
            // DoS guard: stop after 256 candidates so a file full of stray `FF D8 FF` bytes
            // can't build an unbounded list.
            if out.len() >= 256 {
                break;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    out
}

// --- Fujifilm RAF ------------------------------------------------------------

/// RAF stores the embedded JPEG's absolute offset and length as big-endian u32s at fixed
/// header positions 0x54 / 0x58.
fn raf_preview(b: &[u8]) -> Option<(usize, usize)> {
    let off = rd_u32(b, 0x54, false)? as usize;
    let len = rd_u32(b, 0x58, false)? as usize;
    if len == 0 {
        return None;
    }
    Some((off, len))
}

// --- TIFF / EXIF IFD walk ----------------------------------------------------

/// Walk a TIFF-structured raw's directory tree and collect every embedded-JPEG candidate
/// range, returning them alongside the IFD0 `Orientation` (1 if absent). Traverses the IFD0
/// chain, recurses into `SubIFDs` (0x014A) and the EXIF IFD (0x8769), and is bounded against
/// malformed input by a visited-offset set and a hard IFD budget.
fn collect_tiff(b: &[u8]) -> (Vec<(usize, usize)>, u16) {
    let le = b.starts_with(b"II");
    let mut cands = Vec::new();
    let mut orientation = 1u16;
    let mut visited = std::collections::HashSet::new();
    let mut stack = Vec::new();
    if let Some(off0) = rd_u32(b, 4, le) {
        stack.push(off0);
    }
    let mut budget = 64u32; // DoS guard: cap total IFDs walked (real files have 1–3)

    while let Some(ifd_off) = stack.pop() {
        if budget == 0 {
            break;
        }
        if !visited.insert(ifd_off) {
            continue; // already processed (or a malformed cycle)
        }
        budget -= 1;

        let base = ifd_off as usize;
        let count = match rd_u16(b, base, le) {
            Some(c) => c as usize,
            None => continue,
        };
        if count > 4096 {
            continue; // implausible entry count — treat as malformed
        }

        let mut jpeg_off = None;
        let mut jpeg_len = None;
        let mut strip_off = None;
        let mut strip_len = None;
        let mut compression = 0u32;

        for e in 0..count {
            let eo = base + 2 + e * 12;
            let tag = match rd_u16(b, eo, le) {
                Some(t) => t,
                None => break,
            };
            let typ = match rd_u16(b, eo + 2, le) {
                Some(t) => t,
                None => break,
            };
            let n = match rd_u32(b, eo + 4, le) {
                Some(n) => n,
                None => break,
            };

            match tag {
                0x0112 => {
                    // Orientation — keep the first (IFD0's) value.
                    if orientation == 1 {
                        if let Some(v) = entry_scalar(b, eo, typ, le) {
                            orientation = v as u16;
                        }
                    }
                }
                0x0103 => {
                    if let Some(v) = entry_scalar(b, eo, typ, le) {
                        compression = v;
                    }
                }
                0x0111 if n == 1 => strip_off = entry_scalar(b, eo, typ, le),
                0x0117 if n == 1 => strip_len = entry_scalar(b, eo, typ, le),
                0x0201 => jpeg_off = entry_scalar(b, eo, typ, le),
                0x0202 => jpeg_len = entry_scalar(b, eo, typ, le),
                0x014A => {
                    for off in entry_long_array(b, eo, n, le) {
                        stack.push(off);
                    }
                }
                0x8769 => {
                    if let Some(v) = entry_scalar(b, eo, typ, le) {
                        stack.push(v);
                    }
                }
                _ => {}
            }
        }

        // A directory's full-size JPEG preview (Canon/Nikon/Sony thumbnails & previews).
        if let (Some(o), Some(l)) = (jpeg_off, jpeg_len) {
            if l > 0 {
                cands.push((o as usize, l as usize));
            }
        }
        // A single-strip JPEG-compressed image (DNG / some ARW previews live here).
        if compression == 6 || compression == 7 {
            if let (Some(o), Some(l)) = (strip_off, strip_len) {
                if l > 0 {
                    cands.push((o as usize, l as usize));
                }
            }
        }

        // Next directory in the IFD chain.
        if let Some(next) = rd_u32(b, base + 2 + count * 12, le) {
            if next != 0 {
                stack.push(next);
            }
        }
    }

    (cands, orientation)
}

/// Read a single-value IFD entry (`count == 1`) as a u32, honoring BYTE/SHORT/LONG types.
/// The value sits inline in the entry's 4-byte value field (all these types fit).
fn entry_scalar(b: &[u8], eo: usize, typ: u16, le: bool) -> Option<u32> {
    match typ {
        1 => rd_u8(b, eo + 8).map(|v| v as u32),  // BYTE
        3 => rd_u16(b, eo + 8, le).map(|v| v as u32), // SHORT
        _ => rd_u32(b, eo + 8, le),                // LONG (4) and best-effort fallback
    }
}

/// Read a `SubIFDs`-style array of LONG offsets. For `count == 1` the offset is inline; for
/// `count > 1` the value field points at an array of u32s. Capped to bound malformed input.
fn entry_long_array(b: &[u8], eo: usize, n: u32, le: bool) -> Vec<u32> {
    let n = n.min(32) as usize;
    let mut out = Vec::new();
    if n == 0 {
        return out;
    }
    if n == 1 {
        if let Some(v) = rd_u32(b, eo + 8, le) {
            out.push(v);
        }
    } else if let Some(arr_off) = rd_u32(b, eo + 8, le) {
        for i in 0..n {
            if let Some(v) = rd_u32(b, arr_off as usize + i * 4, le) {
                out.push(v);
            }
        }
    }
    out
}

// --- bounds-checked primitive reads ------------------------------------------

fn rd_u8(b: &[u8], o: usize) -> Option<u8> {
    b.get(o).copied()
}

fn rd_u16(b: &[u8], o: usize, le: bool) -> Option<u16> {
    let s = b.get(o..o + 2)?;
    Some(if le {
        u16::from_le_bytes([s[0], s[1]])
    } else {
        u16::from_be_bytes([s[0], s[1]])
    })
}

fn rd_u32(b: &[u8], o: usize, le: bool) -> Option<u32> {
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

    /// Encode a solid-color JPEG of the given size via the `image` crate (test fixtures).
    fn jpeg(w: u32, h: u32, rgb: [u8; 3]) -> Vec<u8> {
        let src = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(w, h, image::Rgb(rgb)));
        let mut buf = std::io::Cursor::new(Vec::new());
        src.write_to(&mut buf, image::ImageFormat::Jpeg).unwrap();
        buf.into_inner()
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
        }
    }

    #[test]
    fn label_maps_extensions_case_insensitively() {
        assert_eq!(label(b"", Some("cr2")), Some("Canon CR2"));
        assert_eq!(label(b"", Some("NEF")), Some("Nikon NEF"));
        assert_eq!(label(b"", Some("dng")), Some("Adobe DNG"));
        assert_eq!(label(b"", Some("png")), None);
        assert_eq!(label(b"", Some("tif")), None); // plain TIFF is not raw
        assert_eq!(label(b"", None), None);
    }

    #[test]
    fn label_detects_magic_without_extension() {
        // Canon CR2 marker in a little-endian TIFF header.
        let mut cr2 = vec![b'I', b'I', 0x2A, 0x00, 0x10, 0, 0, 0];
        cr2.extend_from_slice(b"CR\x02\x00");
        assert_eq!(label(&cr2, None), Some("Canon CR2"));

        // Canon CR3 ISOBMFF brand.
        let mut cr3 = vec![0, 0, 0, 0x18];
        cr3.extend_from_slice(b"ftypcrx ");
        assert_eq!(label(&cr3, None), Some("Canon CR3"));

        assert_eq!(label(b"FUJIFILMCCD-RAW0201", None), Some("Fujifilm RAF"));
    }

    #[test]
    fn jpeg_dimensions_reads_sof() {
        let data = jpeg(640, 480, [10, 20, 30]);
        assert_eq!(jpeg_dimensions(&data), Some((640, 480)));
    }

    #[test]
    fn marker_scan_finds_embedded_jpeg() {
        let preview = jpeg(800, 600, [200, 100, 50]);
        // Bury the JPEG inside junk that contains no FF D8 FF of its own.
        let mut blob = vec![0x11u8; 1000];
        let off = blob.len();
        blob.extend_from_slice(&preview);
        blob.extend_from_slice(&[0x22u8; 500]);

        let found = pick_largest(&blob, &scan_jpeg_markers(&blob)).expect("preview found");
        assert_eq!(found.0, off);
        assert_eq!(jpeg_dimensions(&blob[found.0..found.1]), Some((800, 600)));
    }

    #[test]
    fn picks_largest_of_multiple_previews() {
        let small = jpeg(160, 120, [0, 0, 0]);
        let large = jpeg(1024, 768, [255, 255, 255]);
        let mut blob = vec![0u8; 8];
        let small_off = blob.len();
        blob.extend_from_slice(&small);
        let large_off = blob.len();
        blob.extend_from_slice(&large);

        let cands = vec![(small_off, small.len()), (large_off, large.len())];
        let best = pick_largest(&blob, &cands).unwrap();
        assert_eq!(best.0, large_off);
    }

    /// Build a minimal little-endian TIFF with one IFD that points at an embedded JPEG via
    /// JPEGInterchangeFormat/Length and carries an Orientation tag, then prove `find_preview`
    /// locates the JPEG and reports the orientation.
    #[test]
    fn tiff_ifd_locates_preview_and_orientation() {
        let preview = jpeg(512, 384, [123, 45, 67]);

        // Layout: [8-byte header][IFD][preview JPEG].
        let entries: u16 = 3;
        let ifd_off = 8u32;
        let ifd_len = 2 + entries as usize * 12 + 4;
        let jpeg_off = ifd_off as usize + ifd_len;

        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"II"); // little-endian
        tiff.extend_from_slice(&42u16.to_le_bytes());
        tiff.extend_from_slice(&ifd_off.to_le_bytes());

        // IFD: entry count, then 12-byte entries, then next-IFD offset (0).
        tiff.extend_from_slice(&entries.to_le_bytes());
        let mut entry = |tag: u16, typ: u16, n: u32, val: u32| {
            tiff.extend_from_slice(&tag.to_le_bytes());
            tiff.extend_from_slice(&typ.to_le_bytes());
            tiff.extend_from_slice(&n.to_le_bytes());
            tiff.extend_from_slice(&val.to_le_bytes());
        };
        entry(0x0112, 3, 1, 6); // Orientation = 6 (rotate 90° CW)
        entry(0x0201, 4, 1, jpeg_off as u32); // JPEGInterchangeFormat
        entry(0x0202, 4, 1, preview.len() as u32); // JPEGInterchangeFormatLength
        tiff.extend_from_slice(&0u32.to_le_bytes()); // no next IFD

        assert_eq!(tiff.len(), jpeg_off);
        tiff.extend_from_slice(&preview);

        let p = find_preview(&tiff).expect("preview located");
        assert_eq!(p.orientation, 6);
        assert_eq!(jpeg_dimensions(p.jpeg), Some((512, 384)));
    }

    #[test]
    fn orientation_rotate_90_cw_swaps_axes() {
        // 2x1 image: pixel A then B (left, right).
        let a = [10u8, 11, 12, 255];
        let b = [20u8, 21, 22, 255];
        let mut im = img(2, 1, [a, b].concat());
        apply_orientation(&mut im, 6); // rotate 90° CW -> 1 wide, 2 tall, A on top
        assert_eq!((im.width, im.height), (1, 2));
        assert_eq!(&im.pixels[0..4], &a); // top row
        assert_eq!(&im.pixels[4..8], &b); // bottom row
    }

    #[test]
    fn orientation_mirror_horizontal() {
        let a = [1u8, 1, 1, 255];
        let b = [2u8, 2, 2, 255];
        let mut im = img(2, 1, [a, b].concat());
        apply_orientation(&mut im, 2); // mirror horizontal -> B then A
        assert_eq!((im.width, im.height), (2, 1));
        assert_eq!(&im.pixels[0..4], &b);
        assert_eq!(&im.pixels[4..8], &a);
    }

    #[test]
    fn orientation_identity_and_out_of_range_are_noops() {
        let pixels: Vec<u8> = (0..16).collect();
        for o in [1u16, 0, 9, 999] {
            let mut im = img(2, 2, pixels.clone());
            apply_orientation(&mut im, o);
            assert_eq!(im.pixels, pixels);
            assert_eq!((im.width, im.height), (2, 2));
        }
    }

    #[test]
    fn no_preview_in_garbage_returns_none() {
        let junk = vec![0x55u8; 4096];
        assert!(find_preview(&junk).is_none());
    }
}
