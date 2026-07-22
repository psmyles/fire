//! End-to-end decode tests for OpenEXR, driving in-memory `.exr` files through the public
//! `decode` entry point — the same path the viewer's decode worker takes.
//!
//! EXR is the project's other linear/HDR source (alongside Radiance HDR) and the only backend
//! that reads its headers twice: `decode_exr` parses the metadata on its own first, because the
//! `exr` crate's pixel-buffer closure cannot fail — it allocates 16 bytes per declared pixel and
//! returns the buffer by value, so an oversized header would abort the process there rather than
//! surface an error. These tests pin the ordinary path so that guard can't silently break it.
//!
//! Fixtures are written with the same `exr` crate, as F32 samples, so values round-trip bit-exact.

use std::io::Cursor;

use exr::prelude::*;
use fire_decode::{decode, DecodeOptions, DecodedImage, PixelFormat};

/// A 2x2 RGBA F32 EXR, pixels row-major.
fn exr_2x2(px: [[f32; 4]; 4]) -> Vec<u8> {
    let channels = SpecificChannels::rgba(|pos: Vec2<usize>| {
        let p = px[pos.y() * 2 + pos.x()];
        (p[0], p[1], p[2], p[3])
    });
    let image = Image::from_channels((2, 2), channels);

    let mut buf = Cursor::new(Vec::new());
    image
        .write()
        .to_buffered(&mut buf)
        .expect("write exr fixture");
    buf.into_inner()
}

/// The decoded RGBA f32 pixel at index `i`.
fn pixel(out: &DecodedImage, i: usize) -> [f32; 4] {
    let px: &[f32] = bytemuck::cast_slice(&out.pixels);
    px[i * 4..i * 4 + 4].try_into().unwrap()
}

/// An ordinary EXR decodes to linear 32-bit float RGBA, bit-exact, with the HDR pixel format
/// and the status-bar label the viewer shows.
#[test]
fn exr_decodes_linear_float() {
    let bytes = exr_2x2([
        [1.0, 0.0, 0.0, 1.0],
        [0.0, 0.5, 0.0, 1.0],
        [0.25, 0.25, 0.25, 0.5],
        [0.0, 0.0, 0.0, 0.0],
    ]);
    let out = decode(&bytes, Some("exr"), &DecodeOptions::default()).expect("should decode");

    assert_eq!((out.width, out.height), (2, 2));
    assert_eq!(out.format, PixelFormat::Rgba32Float);
    assert!(
        out.format.is_hdr(),
        "EXR is linear/HDR: exposure + tonemap apply"
    );
    assert_eq!(out.source_format, "OpenEXR");
    assert_eq!(pixel(&out, 0), [1.0, 0.0, 0.0, 1.0]);
    assert_eq!(pixel(&out, 1), [0.0, 0.5, 0.0, 1.0]);
    assert_eq!(pixel(&out, 2), [0.25, 0.25, 0.25, 0.5]);
    assert_eq!(pixel(&out, 3), [0.0, 0.0, 0.0, 0.0]);
}

/// The point of an HDR format: values above 1.0 survive the decode unclamped, so the exposure
/// slider has headroom to pull them back down.
#[test]
fn exr_preserves_values_above_one() {
    let bytes = exr_2x2([[8.0, 4.0, 2.0, 1.0]; 4]);
    let out = decode(&bytes, Some("exr"), &DecodeOptions::default()).expect("should decode");

    for i in 0..4 {
        assert_eq!(
            pixel(&out, i),
            [8.0, 4.0, 2.0, 1.0],
            "HDR values must not clamp to 1.0"
        );
    }
}

/// A partly transparent EXR keeps its alpha, and is *not* flagged `alpha_opaque` — the viewer
/// reads that flag to decide whether the checker backdrop is worth showing.
#[test]
fn exr_alpha_is_preserved_and_not_flagged_opaque() {
    let bytes = exr_2x2([
        [1.0, 1.0, 1.0, 1.0],
        [1.0, 1.0, 1.0, 0.25],
        [1.0, 1.0, 1.0, 1.0],
        [1.0, 1.0, 1.0, 1.0],
    ]);
    let out = decode(&bytes, Some("exr"), &DecodeOptions::default()).expect("should decode");

    assert_eq!(pixel(&out, 1)[3], 0.25);
    assert!(
        !out.alpha_opaque,
        "a transparent sample must clear the opaque-alpha flag"
    );
}

/// A fully opaque EXR is flagged, so the viewer skips the checker backdrop for it.
#[test]
fn exr_fully_opaque_alpha_is_flagged() {
    let bytes = exr_2x2([[0.2, 0.4, 0.6, 1.0]; 4]);
    let out = decode(&bytes, Some("exr"), &DecodeOptions::default()).expect("should decode");
    assert!(out.alpha_opaque);
}

/// EXR routing is by magic bytes (`0x76 2f 31 01`), not extension: a misnamed file still reaches
/// the EXR backend, because the viewer sniffs bytes rather than trusting names.
#[test]
fn exr_routes_by_magic_not_extension() {
    let bytes = exr_2x2([[0.5, 0.5, 0.5, 1.0]; 4]);
    let out = decode(&bytes, Some("png"), &DecodeOptions::default())
        .expect("EXR should decode regardless of the extension hint");
    assert_eq!(out.source_format, "OpenEXR");
}

/// Truncated/garbage EXR data is a clean `Err`, never a panic or an abort — the header pre-read
/// must fail the same way the full read would.
#[test]
fn exr_garbage_errors_cleanly() {
    let mut bytes = exr_2x2([[1.0, 1.0, 1.0, 1.0]; 4]);
    bytes.truncate(bytes.len() / 2);
    assert!(decode(&bytes, Some("exr"), &DecodeOptions::default()).is_err());

    // The magic alone, with no header behind it.
    let stub = [0x76, 0x2f, 0x31, 0x01, 0, 0, 0, 0];
    assert!(decode(&stub, Some("exr"), &DecodeOptions::default()).is_err());
}
