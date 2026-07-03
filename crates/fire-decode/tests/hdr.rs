//! End-to-end decode tests for Radiance HDR, driving hand-crafted RGBE files through the
//! public `decode` entry point — the same path the viewer's decode worker takes.
//!
//! HDR is deliberately routed to the `image` crate, not zune (see `decode_hdr` in lib.rs):
//! zune-hdr ≤ 0.5.2 masks the RGBE exponent shift with `& 31`, so a pixel 32+ stops below
//! unity decodes exactly 2^32 too bright — bright blue/green patches in near-black regions
//! of real skyboxes. The deep-dark test below is the regression guard for that routing.
//!
//! Fixtures are built in-memory: a Radiance header plus flat (uncompressed) RGBE scanlines,
//! which the format uses for images narrower than 8 px. RGBE decodes as
//! `mantissa/256 * 2^(E-128)`, exact in f32, so assertions are bit-exact.

use fire_decode::{decode, DecodeOptions, DecodedImage, PixelFormat};

/// A 2x2 flat-scanline Radiance file: `magic` line, format line, blank line, resolution
/// line, then four RGBE pixels row-major.
fn hdr_2x2(magic: &str, px: [[u8; 4]; 4]) -> Vec<u8> {
    let mut bytes = format!("{magic}\nFORMAT=32-bit_rle_rgbe\n\n-Y 2 +X 2\n").into_bytes();
    for p in px {
        bytes.extend_from_slice(&p);
    }
    bytes
}

/// The decoded RGBA f32 pixel at index `i`.
fn pixel(out: &DecodedImage, i: usize) -> [f32; 4] {
    let px: &[f32] = bytemuck::cast_slice(&out.pixels);
    px[i * 4..i * 4 + 4].try_into().unwrap()
}

/// Ordinary mid-range values decode exactly, with the HDR pixel format, label, and an
/// opaque synthesized alpha lane.
#[test]
fn hdr_decodes_linear_float() {
    let bytes = hdr_2x2(
        "#?RADIANCE",
        [
            [128, 128, 128, 129], // 0.5 * 2^1  = 1.0
            [255, 128, 64, 128],  // 255/256, 0.5, 0.25
            [128, 0, 0, 130],     // 2.0, 0, 0
            [0, 0, 0, 0],         // exponent 0 = true black
        ],
    );
    let out = decode(&bytes, Some("hdr"), &DecodeOptions::default()).expect("should decode");

    assert_eq!((out.width, out.height), (2, 2));
    assert_eq!(out.format, PixelFormat::Rgba32Float);
    assert_eq!(out.source_format, "Radiance HDR");
    assert_eq!(out.channels, 3, "RGBE carries no alpha");
    assert_eq!(pixel(&out, 0), [1.0, 1.0, 1.0, 1.0]);
    assert_eq!(pixel(&out, 1), [255.0 / 256.0, 0.5, 0.25, 1.0]);
    assert_eq!(pixel(&out, 2), [2.0, 0.0, 0.0, 1.0]);
    assert_eq!(pixel(&out, 3), [0.0, 0.0, 0.0, 1.0]);
}

/// Regression guard for the zune-hdr exponent-wrap bug: a pixel 32 stops below unity
/// (E = 96) must decode to ~1.16e-10 — essentially black — not 2^32 times brighter.
/// zune-hdr ≤ 0.5.2 returns 0.5 here.
#[test]
fn hdr_deep_dark_pixels_stay_dark() {
    let deep_dark = [128u8, 128, 128, 96]; // 0.5 * 2^-32
    let bytes = hdr_2x2("#?RADIANCE", [deep_dark; 4]);
    let out = decode(&bytes, Some("hdr"), &DecodeOptions::default()).expect("should decode");

    let expected = 0.5 * (2.0f32).powi(-32);
    for i in 0..4 {
        assert_eq!(pixel(&out, i), [expected, expected, expected, 1.0]);
    }
}

/// The `#?RGBE` signature variant (and, by the same non-strict header parse, old
/// signature-less `.pic` files) routes and decodes like `#?RADIANCE`.
#[test]
fn hdr_rgbe_signature_decodes() {
    let bytes = hdr_2x2("#?RGBE", [[128, 128, 128, 129]; 4]);
    let out = decode(&bytes, Some("hdr"), &DecodeOptions::default()).expect("should decode");

    assert_eq!(out.source_format, "Radiance HDR");
    assert_eq!(pixel(&out, 0), [1.0, 1.0, 1.0, 1.0]);
}

/// HDR routing is by magic bytes, not file extension: a misnamed extension still reaches
/// the HDR backend (the viewer sniffs bytes, not names).
#[test]
fn hdr_routes_by_magic_not_extension() {
    let bytes = hdr_2x2("#?RADIANCE", [[128, 128, 128, 129]; 4]);
    let out = decode(&bytes, Some("png"), &DecodeOptions::default())
        .expect("HDR should decode regardless of the extension hint");
    assert_eq!(out.source_format, "Radiance HDR");
}
