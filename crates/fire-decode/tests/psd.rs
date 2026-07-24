//! End-to-end decode tests for PSD, driving in-memory documents through the public `decode`
//! entry point — the same path the viewer's decode worker takes.
//!
//! PSD is the project's only **C++** FFI boundary (psd_sdk, via `psd-sdk-sys`), and the widest
//! one: `wrapper.cpp` re-samples psd_sdk's *planar* channels into interleaved RGBA itself,
//! branching on bit depth (8/16/32) and on every colour mode Photoshop can save — RGB, greyscale,
//! Indexed, CMYK, Lab — each with or without alpha. Every one of those branches is a hand-written
//! pointer walk, so every one gets a fixture here.
//!
//! Fixtures are built in memory rather than committed as binaries: a minimal PSD is a header plus
//! four length-prefixed sections, and writing it out in code documents the layout the C++ side
//! relies on. Only the merged ("Maximize Compatibility") composite is in scope — the layer stack
//! is a v2 concern and `fire_psd_read_merged` returns error 2 without it.
//!
//! Compiled out without the `psd` feature: there is no psd_sdk to drive then, and the point of
//! that configuration is to build and test on a machine with no vendored native trees at all.
#![cfg(feature = "psd")]

use fire_decode::{decode, DecodeOptions, PixelFormat};

const COLOR_MODE_GRAYSCALE: u16 = 1;
const COLOR_MODE_RGB: u16 = 3;

/// Assemble a minimal PSD: 26-byte header, three empty sections, then the raw (uncompressed)
/// merged image as consecutive channel planes. `planes` are already in the file's big-endian
/// sample order for `depth`.
fn psd(width: u32, height: u32, color_mode: u16, depth: u16, planes: &[Vec<u8>]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(b"8BPS");
    b.extend_from_slice(&1u16.to_be_bytes()); // version 1 (PSD, not PSB)
    b.extend_from_slice(&[0; 6]); // reserved
    b.extend_from_slice(&(planes.len() as u16).to_be_bytes()); // channel count
    b.extend_from_slice(&height.to_be_bytes());
    b.extend_from_slice(&width.to_be_bytes());
    b.extend_from_slice(&depth.to_be_bytes());
    b.extend_from_slice(&color_mode.to_be_bytes());

    b.extend_from_slice(&0u32.to_be_bytes()); // colour mode data: none (RGB/gray need none)
    b.extend_from_slice(&0u32.to_be_bytes()); // image resources: none (so: no ICC)
    b.extend_from_slice(&0u32.to_be_bytes()); // layer + mask info: none

    b.extend_from_slice(&0u16.to_be_bytes()); // image data compression: 0 = raw
    for plane in planes {
        b.extend_from_slice(plane);
    }
    b
}

/// An 8-bit RGB+alpha PSD: the common case, and the one the `hasAlpha` branch in `wrapper.cpp`
/// keys on (colour mode RGB *and* a 4th channel present).
#[test]
fn psd_rgba8_decodes_merged_composite() {
    // 2x2, planar: one plane per channel, row-major within the plane.
    let bytes = psd(
        2,
        2,
        COLOR_MODE_RGB,
        8,
        &[
            vec![255, 0, 0, 10],     // R
            vec![0, 255, 0, 20],     // G
            vec![0, 0, 255, 30],     // B
            vec![255, 255, 128, 40], // A
        ],
    );

    let out = decode(&bytes, Some("psd"), &DecodeOptions::default()).expect("should decode");
    assert_eq!((out.width, out.height), (2, 2));
    assert_eq!(out.format, PixelFormat::Rgba8Unorm);
    assert_eq!(out.source_format, "PSD");
    assert_eq!(out.channels, 4);
    // Planar channels interleave into RGBA, alpha carried through.
    assert_eq!(
        out.pixels,
        vec![255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 128, 10, 20, 30, 40]
    );
    assert!(!out.alpha_opaque, "alpha 128/40 is not opaque");
}

/// An RGB PSD with no alpha channel gets an opaque alpha lane synthesized, and reports 3 channels.
/// `alpha_opaque` stays `false` — it means "a *declared* alpha channel turned out to be opaque",
/// and there is no alpha channel here to declare; the viewer reads `channels` for that.
#[test]
fn psd_rgb8_without_alpha_synthesizes_opaque_lane() {
    let bytes = psd(
        2,
        1,
        COLOR_MODE_RGB,
        8,
        &[vec![200, 10], vec![30, 220], vec![40, 60]],
    );

    let out = decode(&bytes, Some("psd"), &DecodeOptions::default()).expect("should decode");
    assert_eq!((out.width, out.height), (2, 1));
    assert_eq!(out.channels, 3, "a 3-channel PSD reports RGB, not RGBA");
    assert_eq!(out.pixels, vec![200, 30, 40, 255, 10, 220, 60, 255]);
    assert!(
        !out.alpha_opaque,
        "no declared alpha channel -> flag stays false"
    );
}

/// 16-bit PSDs keep 16 bits, and are scaled from Photoshop's range rather than Photoshop's
/// *nominal* one.
///
/// Photoshop stores 16-bit samples as 15-bit+1 integers in the range **0…32768**, not 0…65535 —
/// the vendored SDK says so itself in `PsdParseImageDataSection.cpp`. The wrapper used to narrow
/// with `x >> 8`, which treats 32768 (white) as 128, i.e. rendered every 16-bit document at half
/// brightness. It also flattened the buffer to 8 bits while `bit_depth` went on claiming 16.
#[test]
fn psd_rgb16_scales_from_photoshops_32768_range() {
    let plane16 =
        |vals: [u16; 2]| -> Vec<u8> { vals.iter().flat_map(|v| v.to_be_bytes()).collect() };
    let bytes = psd(
        2,
        1,
        COLOR_MODE_RGB,
        16,
        &[
            plane16([32768, 0]),     // R: white, then black
            plane16([16384, 32768]), // G: half, then white
            plane16([0, 16384]),     // B: black, then half
        ],
    );

    let out = decode(&bytes, Some("psd"), &DecodeOptions::default()).expect("should decode");
    assert_eq!((out.width, out.height), (2, 1));
    assert_eq!(
        out.format,
        PixelFormat::Rgba16Unorm,
        "16-bit source stays 16-bit"
    );
    assert_eq!(out.bit_depth, 16);
    let s: Vec<u16> = out
        .pixels
        .chunks_exact(2)
        .map(|c| u16::from_ne_bytes([c[0], c[1]]))
        .collect();
    // 32768 is full white, not half. 16384 is the midpoint.
    assert_eq!(s, vec![65535, 32768, 0, 65535, 0, 65535, 32768, 65535]);
}

/// 32-bit PSDs are linear/HDR and stay that way: values above 1.0 survive instead of being
/// clamped into an 8-bit lane, so exposure and tonemapping apply as they do for EXR.
#[test]
fn psd_rgb32f_stays_linear_float() {
    let plane32 = |v: f32| -> Vec<u8> { v.to_be_bytes().to_vec() };
    let bytes = psd(
        1,
        1,
        COLOR_MODE_RGB,
        32,
        &[plane32(4.0), plane32(0.5), plane32(0.0)],
    );

    let out = decode(&bytes, Some("psd"), &DecodeOptions::default()).expect("should decode");
    assert_eq!(out.format, PixelFormat::Rgba32Float);
    assert_eq!(out.bit_depth, 32);
    let s: Vec<f32> = out
        .pixels
        .chunks_exact(4)
        .map(|c| f32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    assert_eq!(
        s,
        vec![4.0, 0.5, 0.0, 1.0],
        "HDR range preserved, alpha synthesized opaque"
    );
}

/// A greyscale PSD's alpha channel must survive.
///
/// The wrapper's greyscale branch never read one: `hasAlpha` required colour mode RGB, so a
/// Gray+Alpha document came back fully opaque while still *reporting* 2 channels — the viewer
/// then computed `alpha_opaque` and showed a solid image. A greyscale document whose content
/// lives entirely in alpha (a mask, a texture sheet) rendered as a blank square. Same failure
/// as the TIFF `ExtraSamples` bug, one format over.
#[test]
fn psd_grayscale_alpha_is_not_dropped() {
    let bytes = psd(
        3,
        1,
        COLOR_MODE_GRAYSCALE,
        8,
        &[vec![200, 255, 0], vec![128, 0, 255]],
    );

    let out = decode(&bytes, Some("psd"), &DecodeOptions::default()).expect("should decode");
    assert_eq!(out.channels, 2, "grey + alpha");
    assert_eq!(
        out.pixels,
        vec![200, 200, 200, 128, 255, 255, 255, 0, 0, 0, 0, 255],
        "the alpha plane must reach the alpha lane"
    );
    assert!(!out.alpha_opaque);
}

/// Spot/extra channels must not be counted as image channels, and must not be mistaken for alpha.
///
/// `channels` came straight from the document header, which counts spot channels too — an RGB
/// document with transparency plus one spot channel reported 5. The viewer keys its alpha UI
/// (checker backdrop, alpha isolation, the status label) on 2 or 4, so a file that really did
/// have transparency was presented as having none. Conversely `hasAlpha` was just "is there a
/// 4th plane", so an RGB document with a spot channel and *no* transparency had that spot
/// channel used as opacity.
#[test]
fn psd_spot_channels_do_not_break_the_alpha_report() {
    let rgb = || vec![vec![255u8], vec![0], vec![0]];

    // RGB + alpha + one spot channel: still RGBA, and the alpha is the 4th plane.
    let mut planes = rgb();
    planes.push(vec![128]); // alpha
    planes.push(vec![64]); // spot
    let out = decode(
        &psd(1, 1, COLOR_MODE_RGB, 8, &planes),
        Some("psd"),
        &DecodeOptions::default(),
    )
    .expect("should decode");
    assert_eq!(out.channels, 4, "spot channels are not image channels");
    assert_eq!(out.pixels, vec![255, 0, 0, 128]);

    // RGB with no alpha at all reports 3 and gets an opaque lane.
    let out = decode(
        &psd(1, 1, COLOR_MODE_RGB, 8, &rgb()),
        Some("psd"),
        &DecodeOptions::default(),
    )
    .expect("should decode");
    assert_eq!(out.channels, 3);
    assert_eq!(out.pixels, vec![255, 0, 0, 255]);
}

/// CMYK is stored **inverted** in a PSD (255 = no ink), and the K plane is not optional.
///
/// The wrapper mapped C->R, M->G, Y->B and dropped K entirely, which is wrong twice over: a
/// no-ink (white) document rendered black, and pure cyan rendered red.
#[test]
fn psd_cmyk_composites_through_k() {
    const CMYK: u16 = 4;
    let ink = |v: u8| vec![v];

    // No ink anywhere -> white.
    let white = psd(1, 1, CMYK, 8, &[ink(255), ink(255), ink(255), ink(255)]);
    let out = decode(&white, Some("psd"), &DecodeOptions::default()).expect("should decode");
    assert_eq!(
        out.pixels,
        vec![255, 255, 255, 255],
        "no ink is white, not black"
    );

    // Full black ink only -> black (this is the case K-dropping silently got right).
    let black = psd(1, 1, CMYK, 8, &[ink(255), ink(255), ink(255), ink(0)]);
    let out = decode(&black, Some("psd"), &DecodeOptions::default()).expect("should decode");
    assert_eq!(out.pixels, vec![0, 0, 0, 255]);

    // Full cyan ink, nothing else -> cyan.
    let cyan = psd(1, 1, CMYK, 8, &[ink(0), ink(255), ink(255), ink(255)]);
    let out = decode(&cyan, Some("psd"), &DecodeOptions::default()).expect("should decode");
    assert_eq!(out.pixels, vec![0, 255, 255, 255], "pure cyan, not red");

    // A CMYK document's alpha is its 5th plane, and it reports as colour+alpha.
    let with_alpha = psd(
        1,
        1,
        CMYK,
        8,
        &[ink(255), ink(255), ink(255), ink(255), ink(128)],
    );
    let out = decode(&with_alpha, Some("psd"), &DecodeOptions::default()).expect("should decode");
    assert_eq!(out.channels, 4);
    assert_eq!(out.pixels, vec![255, 255, 255, 128]);
}

/// Lab documents are converted through D50 XYZ rather than having L, a and b read as R, G and B.
#[test]
fn psd_lab_converts_to_srgb() {
    const LAB: u16 = 9;
    // L is 0..255 for L* 0..100; a and b are offset by 128, so 128 is neutral.
    let white = psd(1, 1, LAB, 8, &[vec![255], vec![128], vec![128]]);
    let out = decode(&white, Some("psd"), &DecodeOptions::default()).expect("should decode");
    assert_eq!(
        out.pixels,
        vec![255, 255, 255, 255],
        "L*=100 neutral is white"
    );

    let black = psd(1, 1, LAB, 8, &[vec![0], vec![128], vec![128]]);
    let out = decode(&black, Some("psd"), &DecodeOptions::default()).expect("should decode");
    assert_eq!(out.pixels, vec![0, 0, 0, 255], "L*=0 neutral is black");

    // L* = 50 is perceptual middle grey, which is sRGB ~119 — NOT 128.
    let mid = psd(1, 1, LAB, 8, &[vec![128], vec![128], vec![128]]);
    let out = decode(&mid, Some("psd"), &DecodeOptions::default()).expect("should decode");
    let v = out.pixels[0];
    assert!(
        (117..=121).contains(&v),
        "L*~50 should land near sRGB 119, got {v}"
    );
    assert_eq!(out.pixels[1], v, "neutral stays neutral");
    assert_eq!(out.pixels[2], v);
}

/// A Bitmap-mode (1-bit) PSD is refused rather than sampled.
///
/// psd_sdk sizes its planes with `bitsPerChannel / 8`, which is **zero** at 1 bit, so every plane
/// is a zero-byte allocation. The old sampler had no depth check and fell through to its 32-bit
/// float branch, reading `4 * w * h` bytes off the end of it.
#[test]
fn psd_one_bit_bitmap_mode_is_refused() {
    const BITMAP: u16 = 0;
    let bytes = psd(8, 8, BITMAP, 1, &[vec![0xff; 8]]);
    assert!(
        decode(&bytes, Some("psd"), &DecodeOptions::default()).is_err(),
        "1-bit documents have no readable planes and must be refused, not read past"
    );
}

/// The grayscale branch: a single plane is replicated across R, G and B rather than being read
/// as a red channel with two missing ones.
#[test]
fn psd_grayscale_replicates_to_rgb() {
    let bytes = psd(3, 1, COLOR_MODE_GRAYSCALE, 8, &[vec![0, 128, 255]]);

    let out = decode(&bytes, Some("psd"), &DecodeOptions::default()).expect("should decode");
    assert_eq!((out.width, out.height), (3, 1));
    assert_eq!(
        out.pixels,
        vec![0, 0, 0, 255, 128, 128, 128, 255, 255, 255, 255, 255],
        "gray value must land on all three colour channels"
    );
}

/// PSD routing is by magic bytes (`8BPS`), not extension.
#[test]
fn psd_routes_by_magic_not_extension() {
    let bytes = psd(1, 1, COLOR_MODE_RGB, 8, &[vec![7], vec![8], vec![9]]);
    let out = decode(&bytes, Some("png"), &DecodeOptions::default())
        .expect("PSD should decode regardless of the extension hint");
    assert_eq!(out.source_format, "PSD");
}

/// A truncated PSD must be refused, not rendered.
///
/// psd_sdk follows the offsets in the file's own header and never learns it read past the end, so
/// short reads used to be served as *uninitialized* heap memory and sampled straight into the
/// composite — a malformed file could paint whatever the process happened to have on the heap.
/// `MemoryFile` now zero-fills any short read and records it, and `fire_psd_open` rejects the
/// document. Regression guard for both halves of that.
#[test]
fn psd_truncated_is_rejected_not_rendered_from_uninitialized_memory() {
    // Valid signature, nothing behind it.
    assert!(decode(b"8BPS", Some("psd"), &DecodeOptions::default()).is_err());

    // Well-formed 64x64 header, image data cut off entirely.
    let full = vec![vec![1u8; 4096], vec![1u8; 4096], vec![1u8; 4096]];
    let mut truncated = psd(64, 64, COLOR_MODE_RGB, 8, &full);
    truncated.truncate(30);
    assert!(
        decode(&truncated, Some("psd"), &DecodeOptions::default()).is_err(),
        "a truncated PSD must error, never decode from whatever was in the buffer"
    );

    // Image data present but one plane short: still a short read, still refused.
    let two_planes = vec![vec![9u8; 4], vec![9u8; 4]];
    let mut missing_plane = psd(2, 2, COLOR_MODE_RGB, 8, &two_planes);
    missing_plane[12..14].copy_from_slice(&3u16.to_be_bytes()); // header claims 3 channels
    assert!(decode(&missing_plane, Some("psd"), &DecodeOptions::default()).is_err());
}

/// A PSD decode bomb: a 40-byte file whose header claims 60000x60000. psd_sdk allocates its planar
/// channel buffers from that header inside `fire_psd_open` — ~10 GB — before Rust sees a single
/// dimension, and a failed allocation aborts the process rather than unwinding. The guard has to
/// be at the C++ allocation site, and this is the test that it is.
#[test]
fn psd_decode_bomb_header_is_rejected() {
    let small = vec![vec![0u8; 4], vec![0u8; 4], vec![0u8; 4]];
    let mut liar = psd(2, 2, COLOR_MODE_RGB, 8, &small);
    liar[14..18].copy_from_slice(&60_000u32.to_be_bytes()); // height
    liar[18..22].copy_from_slice(&60_000u32.to_be_bytes()); // width

    assert!(
        decode(&liar, Some("psd"), &DecodeOptions::default()).is_err(),
        "a 60000x60000 header on a 40-byte file must be refused before psd_sdk allocates"
    );
}
