//! End-to-end decode tests for PSD, driving in-memory documents through the public `decode`
//! entry point — the same path the viewer's decode worker takes.
//!
//! PSD is the project's only **C++** FFI boundary (psd_sdk, via `psd-sdk-sys`), and the widest
//! one: `wrapper.cpp` re-samples psd_sdk's *planar* channels into interleaved RGBA itself,
//! branching on bit depth (8/16/32) and colour mode (RGB/grayscale, with or without alpha). Each
//! of those branches is a hand-written pointer walk, so each gets a fixture here.
//!
//! Fixtures are built in memory rather than committed as binaries: a minimal PSD is a header plus
//! four length-prefixed sections, and writing it out in code documents the layout the C++ side
//! relies on. Only the merged ("Maximize Compatibility") composite is in scope — the layer stack
//! is a v2 concern and `fire_psd_read_merged_rgba8` returns error 2 without it.

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

/// The 16-bit branch of `sample_channel`: psd_sdk hands back native `uint16` samples and the
/// wrapper narrows them to 8-bit with `>> 8`, so the high byte is what survives.
#[test]
fn psd_rgb16_narrows_to_high_byte() {
    // Big-endian 16-bit samples: 0xABCD -> 0xAB, 0x1234 -> 0x12, 0xFF00 -> 0xFF.
    let plane16 =
        |vals: [u16; 2]| -> Vec<u8> { vals.iter().flat_map(|v| v.to_be_bytes()).collect() };
    let bytes = psd(
        2,
        1,
        COLOR_MODE_RGB,
        16,
        &[
            plane16([0xABCD, 0x1234]), // R
            plane16([0xFF00, 0x0000]), // G
            plane16([0x8080, 0xFFFF]), // B
        ],
    );

    let out = decode(&bytes, Some("psd"), &DecodeOptions::default()).expect("should decode");
    assert_eq!((out.width, out.height), (2, 1));
    assert_eq!(
        out.pixels,
        vec![0xAB, 0xFF, 0x80, 255, 0x12, 0x00, 0xFF, 255]
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
