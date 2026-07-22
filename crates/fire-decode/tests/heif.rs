//! End-to-end decode tests for the libheif backend (AVIF/HEIC/HEIF), driving real
//! container files through the public `decode` entry point — the same path the viewer's
//! decode worker takes.
//!
//! Fixtures are tiny 16x16 images encoded once (AVIF via Pillow's native AV1 encoder, HEIC
//! via pillow-heif's HEVC encoder). Both are 8-bit, so there is no 10/12-bit HDR fixture
//! here; that 16-bit scaling path is covered by code review. AVIF exercises the dav1d codec
//! path, HEIC the libde265 path; both share the same wrapper, routing, RGBA conversion, and
//! alpha handling.
//!
//! Color is asserted within a small tolerance: even "max quality" HEIF/AVIF round-trips
//! through a RGB->YUV->RGB matrix conversion that rounds a channel by a unit or two, so
//! bit-exact assertions would be brittle. Alpha is a separate (non-YUV) plane and stays
//! effectively exact.

use fire_decode::{decode, DecodeOptions, DecodedImage, PixelFormat};

/// Assert the top-left RGBA pixel is within `tol` of `expected` on every channel.
fn assert_top_left(out: &DecodedImage, expected: [u8; 4], tol: i32) {
    let got = [out.pixels[0], out.pixels[1], out.pixels[2], out.pixels[3]];
    for (i, (g, e)) in got.iter().zip(&expected).enumerate() {
        assert!(
            (*g as i32 - *e as i32).abs() <= tol,
            "channel {i}: got {g}, expected ~{e} (tol {tol}); full pixel {got:?} vs {expected:?}"
        );
    }
}

/// A solid AVIF decodes through libheif/dav1d to RGBA8 with the right dims, status label,
/// channel count, and (near-exact) color.
#[test]
fn avif_solid_decodes() {
    let bytes = include_bytes!("fixtures/solid.avif");
    let out =
        decode(bytes, Some("avif"), &DecodeOptions::default()).expect("solid.avif should decode");

    assert_eq!((out.width, out.height), (16, 16));
    assert_eq!(out.format, PixelFormat::Rgba8Unorm);
    assert_eq!(out.source_format, "AVIF");
    assert_eq!(out.channels, 3, "no alpha channel in this fixture");
    assert_top_left(&out, [200, 30, 40, 255], 3);
}

/// An AVIF with an alpha channel reports 4 source channels and preserves the alpha value.
#[test]
fn avif_alpha_decodes() {
    let bytes = include_bytes!("fixtures/alpha.avif");
    let out =
        decode(bytes, Some("avif"), &DecodeOptions::default()).expect("alpha.avif should decode");

    assert_eq!((out.width, out.height), (16, 16));
    assert_eq!(out.format, PixelFormat::Rgba8Unorm);
    assert_eq!(out.source_format, "AVIF");
    assert_eq!(out.channels, 4, "fixture carries an alpha channel");
    assert_top_left(&out, [20, 180, 90, 128], 3);
}

/// AVIF routing is by ISOBMFF `ftyp` brand, not file extension: a misnamed extension still
/// reaches the libheif backend (the viewer sniffs bytes, not names).
#[test]
fn avif_routes_by_magic_not_extension() {
    let bytes = include_bytes!("fixtures/solid.avif");
    let out = decode(bytes, Some("png"), &DecodeOptions::default())
        .expect("AVIF should decode regardless of the extension hint");
    assert_eq!(out.source_format, "AVIF");
}

/// A solid HEIC decodes through libheif/libde265 to RGBA8 with the right dims, "HEIC"
/// label, and (near-exact) color.
#[test]
fn heic_solid_decodes() {
    let bytes = include_bytes!("fixtures/solid.heic");
    let out =
        decode(bytes, Some("heic"), &DecodeOptions::default()).expect("solid.heic should decode");

    assert_eq!((out.width, out.height), (16, 16));
    assert_eq!(out.format, PixelFormat::Rgba8Unorm);
    assert_eq!(out.source_format, "HEIC");
    assert_eq!(out.channels, 3);
    assert_top_left(&out, [200, 30, 40, 255], 3);
}

/// A HEIC with an alpha channel reports 4 source channels and preserves the alpha value.
#[test]
fn heic_alpha_decodes() {
    let bytes = include_bytes!("fixtures/alpha.heic");
    let out =
        decode(bytes, Some("heic"), &DecodeOptions::default()).expect("alpha.heic should decode");

    assert_eq!((out.width, out.height), (16, 16));
    assert_eq!(out.source_format, "HEIC");
    assert_eq!(out.channels, 4, "fixture carries an alpha channel");
    assert_top_left(&out, [20, 180, 90, 128], 3);
}
