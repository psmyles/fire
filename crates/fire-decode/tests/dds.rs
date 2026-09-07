//! End-to-end decode tests for DDS, driving files built in memory through the public `decode`
//! entry point — the same path the viewer's decode worker takes.
//!
//! Fixtures are assembled with `ddsfile`'s own writer (which allocates a correctly sized,
//! zero-filled data buffer for the format and dimensions asked for) and then filled with
//! hand-written blocks, so a test says exactly which bytes produced which pixels. The two
//! guard tests bypass the writer and lay out the 128-byte header by hand, because the point of
//! those is a header whose numbers the writer would refuse to allocate for.

use ddsfile::{
    AlphaMode, D3D10ResourceDimension, D3DFormat, Dds, DxgiFormat, NewD3dParams, NewDxgiParams,
};
use fire_decode::{decode, DecodeError, DecodeOptions, DecodedImage, PixelFormat, SheetKind};

// --- fixture helpers -------------------------------------------------------------------------

/// Serialize a `Dds` to the bytes `decode` will see.
fn bytes(dds: &Dds) -> Vec<u8> {
    let mut out = Vec::new();
    dds.write(&mut out).expect("DDS writes");
    out
}

/// A single-surface DX10 file of `format`, with `data` as its payload.
fn dxgi(format: DxgiFormat, width: u32, height: u32, data: &[u8]) -> Vec<u8> {
    let mut dds = Dds::new_dxgi(NewDxgiParams {
        height,
        width,
        depth: None,
        format,
        mipmap_levels: None,
        array_layers: None,
        is_cubemap: false,
        resource_dimension: D3D10ResourceDimension::Texture2D,
        alpha_mode: AlphaMode::Straight,
        caps2: Default::default(),
    })
    .expect("DX10 header");
    dds.data[..data.len()].copy_from_slice(data);
    bytes(&dds)
}

/// A single-surface legacy D3D9 file of `format`, with `data` as its payload.
fn d3d(format: D3DFormat, width: u32, height: u32, data: &[u8]) -> Vec<u8> {
    let mut dds = Dds::new_d3d(NewD3dParams {
        height,
        width,
        depth: None,
        format,
        mipmap_levels: None,
        caps2: Default::default(),
    })
    .expect("D3D9 header");
    dds.data[..data.len()].copy_from_slice(data);
    bytes(&dds)
}

/// One BC1 colour block: both endpoints `colour` (RGB565) and every index 0, so all sixteen
/// texels come back as endpoint 0. `c0 > c1` selects the four-colour opaque mode.
fn bc1_solid(colour: u16) -> [u8; 8] {
    let mut b = [0u8; 8];
    b[0..2].copy_from_slice(&colour.to_le_bytes());
    b[2..4].copy_from_slice(&0u16.to_le_bytes());
    b
}

/// One BC4/BC5-half block: `a0` on every texel. `a0 > a1` selects the eight-value mode, and an
/// all-zero index word picks `a0` throughout.
fn bc4_solid(a0: u8) -> [u8; 8] {
    let mut b = [0u8; 8];
    b[0] = a0;
    b[1] = 0;
    b
}

/// The decoded RGBA8 texel at index `i`.
fn rgba8(img: &DecodedImage, i: usize) -> [u8; 4] {
    img.pixels[i * 4..i * 4 + 4].try_into().unwrap()
}

/// The decoded RGBA16 texel at index `i`, as raw 16-bit lanes (half-float bits, or unorm).
fn lanes16(img: &DecodedImage, i: usize) -> [u16; 4] {
    let px: &[u16] = bytemuck::cast_slice(&img.pixels);
    px[i * 4..i * 4 + 4].try_into().unwrap()
}

fn opts() -> DecodeOptions {
    DecodeOptions::default()
}

// --- block-compressed formats ----------------------------------------------------------------

/// The base case: a BC1 block whose endpoints are both pure red fills the surface with red, and
/// the metadata describes the encoding rather than the buffer we normalized it into.
#[test]
fn bc1_solid_block_decodes_to_its_endpoint_colour() {
    // RGB565 with all five red bits set; bcdec widens 5-bit 31 to 8-bit 255.
    let file = dxgi(DxgiFormat::BC1_UNorm, 4, 4, &bc1_solid(0xf800));
    let img = decode(&file, Some("dds"), &opts()).expect("BC1 decodes");

    assert_eq!((img.width, img.height), (4, 4));
    assert_eq!(img.format, PixelFormat::Rgba8Unorm);
    assert_eq!(img.source_format, "DDS BC1");
    assert_eq!(img.bit_depth, 8);
    assert_eq!(img.channels, 4);
    assert_eq!(img.pixels.len(), 4 * 4 * 4);
    for i in 0..16 {
        assert_eq!(rgba8(&img, i), [255, 0, 0, 255], "texel {i}");
    }
    // Every alpha sample is opaque, so the viewer skips the checker backdrop.
    assert!(img.alpha_opaque);
}

/// BC4 carries one channel. Showing it in the red lane alone would render a height map or a
/// roughness mask as a red wash, so it is replicated to grey — and reported as one channel.
#[test]
fn bc4_replicates_its_one_channel_to_grey() {
    let file = dxgi(DxgiFormat::BC4_UNorm, 4, 4, &bc4_solid(200));
    let img = decode(&file, Some("dds"), &opts()).expect("BC4 decodes");

    assert_eq!(img.source_format, "DDS BC4");
    assert_eq!(img.channels, 1);
    for i in 0..16 {
        assert_eq!(rgba8(&img, i), [200, 200, 200, 255], "texel {i}");
    }
}

/// BC5 is two independent BC4 blocks — the normal-map format. Both land in their own lane with
/// blue zeroed, which is how the file was authored; reconstructing Z is an edit, not a view.
#[test]
fn bc5_lands_in_the_red_and_green_lanes() {
    let mut data = [0u8; 16];
    data[..8].copy_from_slice(&bc4_solid(200));
    data[8..].copy_from_slice(&bc4_solid(100));
    let file = dxgi(DxgiFormat::BC5_UNorm, 4, 4, &data);
    let img = decode(&file, Some("dds"), &opts()).expect("BC5 decodes");

    assert_eq!(img.source_format, "DDS BC5");
    // Three, not two: a two-lane source shown as RGB is not "Gray+A".
    assert_eq!(img.channels, 3);
    for i in 0..16 {
        assert_eq!(rgba8(&img, i), [200, 100, 0, 255], "texel {i}");
    }
}

/// A signed BC5 is re-centred rather than reinterpreted. `bcdec` hands back the raw -127..127
/// range as bytes, so without this a signed normal map shows as noise: zero must land on
/// mid-grey, not on black.
#[test]
fn signed_block_channels_are_recentred_on_mid_grey() {
    let mut data = [0u8; 16];
    data[..8].copy_from_slice(&bc4_solid(0)); // 0.0 -> mid grey
    data[8..].copy_from_slice(&bc4_solid(127u8)); // +1.0 -> full
    let file = dxgi(DxgiFormat::BC5_SNorm, 4, 4, &data);
    let img = decode(&file, Some("dds"), &opts()).expect("signed BC5 decodes");

    let [r, g, b, a] = rgba8(&img, 0);
    assert_eq!((r, g, b, a), (128, 255, 0, 255));
}

/// BC6H is the HDR block format, and the first decoder in the crate to produce
/// `Rgba16Float` — the layout the renderer has always understood but nothing emitted. It
/// carries no alpha, so an opaque lane is synthesized.
#[test]
fn bc6h_decodes_to_half_float_hdr() {
    let file = dxgi(DxgiFormat::BC6H_UF16, 4, 4, &[0u8; 16]);
    let img = decode(&file, Some("dds"), &opts()).expect("BC6H decodes");

    assert_eq!(img.format, PixelFormat::Rgba16Float);
    assert!(img.format.is_hdr(), "BC6H drives the exposure/tonemap path");
    assert_eq!(img.source_format, "DDS BC6H");
    assert_eq!(img.bit_depth, 16);
    assert_eq!(img.channels, 3);
    assert_eq!(img.pixels.len(), 4 * 4 * 8);
    for i in 0..16 {
        // 0x3c00 is half-precision 1.0.
        assert_eq!(lanes16(&img, i)[3], 0x3c00, "alpha of texel {i}");
    }
}

/// BC7 is the modern 8-bit block format; all that is asserted here is that it routes to its own
/// decoder and reports itself, since a hand-written BC7 block is not a readable fixture.
#[test]
fn bc7_routes_to_its_own_decoder() {
    let file = dxgi(DxgiFormat::BC7_UNorm, 4, 4, &[0u8; 16]);
    let img = decode(&file, Some("dds"), &opts()).expect("BC7 decodes");

    assert_eq!(img.source_format, "DDS BC7");
    assert_eq!(img.format, PixelFormat::Rgba8Unorm);
    assert_eq!(img.pixels.len(), 4 * 4 * 4);
}

/// DDS rounds a block surface up to whole 4x4 blocks, so a 5x3 image is stored as 2x1 blocks
/// with eleven texels of padding. Cropping that back has to not read — or write — past the end.
#[test]
fn a_non_multiple_of_four_surface_is_cropped_not_padded() {
    let mut data = [0u8; 16];
    data[..8].copy_from_slice(&bc1_solid(0xf800)); // left block: red
    data[8..].copy_from_slice(&bc1_solid(0x001f)); // right block: blue
    let file = dxgi(DxgiFormat::BC1_UNorm, 5, 3, &data);
    let img = decode(&file, Some("dds"), &opts()).expect("5x3 BC1 decodes");

    assert_eq!((img.width, img.height), (5, 3));
    assert_eq!(img.pixels.len(), 5 * 3 * 4);
    // Column 4 is the single texel that comes from the second block.
    assert_eq!(rgba8(&img, 0), [255, 0, 0, 255]);
    assert_eq!(rgba8(&img, 3), [255, 0, 0, 255]);
    assert_eq!(rgba8(&img, 4), [0, 0, 255, 255]);
    // Last row, last column: still the second block, and in bounds.
    assert_eq!(rgba8(&img, 5 * 3 - 1), [0, 0, 255, 255]);
}

// --- uncompressed formats --------------------------------------------------------------------

/// A DX10 BGRA file names a channel order rather than bit masks; the lanes still have to come
/// out in RGBA order.
#[test]
fn bgra8_is_swizzled_to_rgba() {
    // Two texels, stored B, G, R, A.
    let data = [10u8, 20, 30, 40, 50, 60, 70, 80];
    let file = dxgi(DxgiFormat::B8G8R8A8_UNorm, 2, 1, &data);
    let img = decode(&file, Some("dds"), &opts()).expect("BGRA8 decodes");

    assert_eq!(img.source_format, "DDS BGRA8");
    assert_eq!(img.format, PixelFormat::Rgba8Unorm);
    assert_eq!(img.channels, 4);
    assert_eq!(rgba8(&img, 0), [30, 20, 10, 40]);
    assert_eq!(rgba8(&img, 1), [70, 60, 50, 80]);
}

/// Half-float channels stay half-float: this is an HDR source and must reach the exposure and
/// tonemap controls rather than being flattened to 8 bits.
#[test]
fn rgba16f_stays_half_float() {
    // 1.0, 0.5, 2.0, 1.0 in half precision.
    let halves: [u16; 4] = [0x3c00, 0x3800, 0x4000, 0x3c00];
    let mut data = Vec::new();
    for h in halves {
        data.extend_from_slice(&h.to_le_bytes());
    }
    let file = dxgi(DxgiFormat::R16G16B16A16_Float, 1, 1, &data);
    let img = decode(&file, Some("dds"), &opts()).expect("RGBA16F decodes");

    assert_eq!(img.format, PixelFormat::Rgba16Float);
    assert!(img.format.is_hdr());
    assert_eq!(img.source_format, "DDS RGBA16F");
    assert_eq!(lanes16(&img, 0), halves);
}

/// A 16-bit unsigned source keeps all sixteen bits rather than being narrowed to 8.
#[test]
fn rgba16_unorm_keeps_its_depth() {
    let lanes: [u16; 4] = [0xffff, 0x8000, 0x0001, 0xffff];
    let mut data = Vec::new();
    for v in lanes {
        data.extend_from_slice(&v.to_le_bytes());
    }
    let file = dxgi(DxgiFormat::R16G16B16A16_UNorm, 1, 1, &data);
    let img = decode(&file, Some("dds"), &opts()).expect("RGBA16 decodes");

    assert_eq!(img.format, PixelFormat::Rgba16Unorm);
    assert_eq!(img.bit_depth, 16);
    assert_eq!(lanes16(&img, 0), lanes);
}

/// The legacy D3D9 path: no DX10 header at all, the layout described only by bit masks. The
/// mask-driven expander is what covers every exporter that never moved to DXGI.
#[test]
fn legacy_a8r8g8b8_decodes_through_the_bit_masks() {
    // A8R8G8B8 is a little-endian word with blue in the low byte: stored B, G, R, A.
    let data = [30u8, 20, 10, 200];
    let file = d3d(D3DFormat::A8R8G8B8, 1, 1, &data);
    let img = decode(&file, Some("dds"), &opts()).expect("A8R8G8B8 decodes");

    assert_eq!(img.format, PixelFormat::Rgba8Unorm);
    assert_eq!(img.channels, 4);
    assert_eq!(rgba8(&img, 0), [10, 20, 30, 200]);
}

/// A 10-bit-per-channel source is widened to 16, not crushed to 8: the extra precision is the
/// only reason to author one.
#[test]
fn ten_bit_channels_widen_to_sixteen() {
    // Packed low-to-high: red full, green empty, blue mid, alpha full.
    let (r, g, b, a) = (1023u32, 0u32, 511u32, 3u32);
    let word: u32 = r | (g << 10) | (b << 20) | (a << 30);
    let file = dxgi(DxgiFormat::R10G10B10A2_UNorm, 1, 1, &word.to_le_bytes());
    let img = decode(&file, Some("dds"), &opts()).expect("RGB10A2 decodes");

    assert_eq!(img.format, PixelFormat::Rgba16Unorm);
    assert_eq!(img.bit_depth, 10);
    let [r, g, b, a] = lanes16(&img, 0);
    assert_eq!((r, g, a), (0xffff, 0, 0xffff));
    // 511/1023 of 65535 is 32735.03, and the rounding lands it on 32735.
    assert_eq!(b, 32735);
}

/// A single-channel float is a mask or a depth buffer, and reads as grey.
#[test]
fn single_channel_float_replicates_to_grey() {
    let file = dxgi(DxgiFormat::R32_Float, 1, 1, &0.25f32.to_le_bytes());
    let img = decode(&file, Some("dds"), &opts()).expect("R32F decodes");

    assert_eq!(img.format, PixelFormat::Rgba32Float);
    assert_eq!(img.channels, 1);
    let px: &[f32] = bytemuck::cast_slice(&img.pixels);
    assert_eq!(px[..4], [0.25, 0.25, 0.25, 1.0]);
}

// --- the file's own mip chain ------------------------------------------------------------------

/// A DDS with mips hands its own levels over rather than letting the viewer compute them. The
/// levels in a `.dds` are authored — often with a filter and a gamma a box filter will not
/// reproduce — and an artist opening one wants to see what shipped.
#[test]
fn an_authored_mip_chain_is_carried_through() {
    // 8x8 BC1 with all four levels: 4, 1, 1, 1 blocks. Each level a different colour, so a
    // recomputed chain (which would blend towards level 0's red) is distinguishable.
    let mut dds = Dds::new_dxgi(NewDxgiParams {
        height: 8,
        width: 8,
        depth: None,
        format: DxgiFormat::BC1_UNorm,
        mipmap_levels: Some(4),
        array_layers: None,
        is_cubemap: false,
        resource_dimension: D3D10ResourceDimension::Texture2D,
        alpha_mode: AlphaMode::Straight,
        caps2: Default::default(),
    })
    .expect("mipped header");
    let colours = [0xf800u16, 0x07e0, 0x001f, 0xf800];
    let mut at = 0;
    for (level, colour) in colours.iter().enumerate() {
        let blocks = if level == 0 { 4 } else { 1 };
        for _ in 0..blocks {
            dds.data[at..at + 8].copy_from_slice(&bc1_solid(*colour));
            at += 8;
        }
    }
    let img = decode(&bytes(&dds), Some("dds"), &opts()).expect("mipped BC1 decodes");

    let mips = img.source_mips.expect("the file's chain came along");
    assert_eq!(mips.len(), 3, "levels 1..3 of an 8x8 chain");
    // Level 1 is 4x4 green, level 2 is 2x2 blue, level 3 is 1x1 red — exactly as authored.
    assert_eq!(mips[0].len(), 4 * 4 * 4);
    assert_eq!(mips[0][..4], [0, 255, 0, 255]);
    assert_eq!(mips[1].len(), 2 * 2 * 4);
    assert_eq!(mips[1][..4], [0, 0, 255, 255]);
    assert_eq!(mips[2].len(), 4);
    assert_eq!(mips[2][..4], [255, 0, 0, 255]);
}

/// A DDS without mips reports none, and the viewer builds the chain as it always has.
#[test]
fn a_single_level_file_supplies_no_chain() {
    let file = dxgi(DxgiFormat::BC1_UNorm, 4, 4, &bc1_solid(0xf800));
    let img = decode(&file, Some("dds"), &opts()).expect("BC1 decodes");
    assert!(img.source_mips.is_none());
}

/// Anything that rewrites the canvas invalidates the chain, because those levels are levels *of*
/// the canvas that was replaced. Downscaling is the reachable case: a nearest-neighbour resample
/// is not what produced the file's level 1, so keeping it would show the wrong pixels when
/// zoomed out.
#[test]
fn a_downscale_drops_the_file_chain() {
    let mut dds = Dds::new_dxgi(NewDxgiParams {
        height: 8,
        width: 8,
        depth: None,
        format: DxgiFormat::BC1_UNorm,
        mipmap_levels: Some(4),
        array_layers: None,
        is_cubemap: false,
        resource_dimension: D3D10ResourceDimension::Texture2D,
        alpha_mode: AlphaMode::Straight,
        caps2: Default::default(),
    })
    .expect("mipped header");
    for chunk in dds.data.chunks_mut(8) {
        chunk.copy_from_slice(&bc1_solid(0xf800));
    }
    let file = bytes(&dds);

    let big = decode(&file, Some("dds"), &opts()).expect("decodes");
    assert!(big.source_mips.is_some(), "kept at full size");

    let shrunk = DecodeOptions {
        max_dim: 4,
        ..DecodeOptions::default()
    };
    let small = decode(&file, Some("dds"), &shrunk).expect("decodes");
    assert_eq!((small.width, small.height), (4, 4));
    assert_eq!(small.downscaled_from, Some((8, 8)));
    assert!(
        small.source_mips.is_none(),
        "a rewritten canvas invalidates the file's levels"
    );
}

// --- cubemaps, arrays and volumes ---------------------------------------------------------------

/// A multi-surface DX10 file: `layers` array layers (times six if `cube`) and `depth` slices.
fn multi(
    format: DxgiFormat,
    width: u32,
    height: u32,
    layers: u32,
    cube: bool,
    depth: Option<u32>,
    levels: Option<u32>,
) -> Dds {
    Dds::new_dxgi(NewDxgiParams {
        height,
        width,
        depth,
        format,
        mipmap_levels: levels,
        array_layers: Some(layers),
        is_cubemap: cube,
        resource_dimension: if depth.is_some() {
            D3D10ResourceDimension::Texture3D
        } else {
            D3D10ResourceDimension::Texture2D
        },
        alpha_mode: AlphaMode::Straight,
        caps2: Default::default(),
    })
    .expect("multi-surface header")
}

/// Fill `dds.data` with one solid BC1 block per 8 bytes, cycling through `colours`.
fn fill_blocks(dds: &mut Dds, colours: &[u16]) {
    for (i, chunk) in dds.data.chunks_mut(8).enumerate() {
        chunk.copy_from_slice(&bc1_solid(colours[i % colours.len()]));
    }
}

/// A cubemap's six faces are tiled into one near-square sheet, in the DDS face order, and the
/// layout says how to read them back. A single 24576-wide strip would be the obvious alternative
/// and nearly a decode-guard rejection at 4096 per face.
#[test]
fn a_cubemap_tiles_its_six_faces_into_a_sheet() {
    let mut dds = multi(DxgiFormat::BC1_UNorm, 4, 4, 6, true, None, None);
    // One distinct colour per face, so a mis-ordered or mis-placed face is visible.
    let colours = [0xf800u16, 0x07e0, 0x001f, 0xffe0, 0xf81f, 0x07ff];
    fill_blocks(&mut dds, &colours);
    let img = decode(&bytes(&dds), Some("dds"), &opts()).expect("cubemap decodes");

    let l = img.layout.expect("a cubemap is a sheet");
    assert_eq!((l.cols, l.rows, l.frames), (3, 2, 6));
    assert_eq!(l.kind, SheetKind::CubeFaces);
    assert_eq!((img.width, img.height), (12, 8), "3x2 grid of 4x4 faces");

    // Face k sits at cell k, row-major. Check each one's top-left texel.
    let expected = [
        [255, 0, 0, 255],
        [0, 255, 0, 255],
        [0, 0, 255, 255],
        [255, 255, 0, 255],
        [255, 0, 255, 255],
        [0, 255, 255, 255],
    ];
    for (face, want) in expected.iter().enumerate() {
        let (cx, cy) = (face as u32 % 3, face as u32 / 3);
        let i = (cy * 4 * 12 + cx * 4) as usize;
        assert_eq!(&rgba8(&img, i), want, "face {face}");
    }
}

/// A texture array is the same mechanism with a different name, and a count that need not fill
/// the grid: five layers tile 3x2 with one cell left over, and `frames` says so, so the viewer's
/// transport does not step onto the empty one.
#[test]
fn an_array_reports_its_real_layer_count_not_the_grid_size() {
    let mut dds = multi(DxgiFormat::BC1_UNorm, 4, 4, 5, false, None, None);
    fill_blocks(&mut dds, &[0xf800]);
    let img = decode(&bytes(&dds), Some("dds"), &opts()).expect("array decodes");

    let l = img.layout.expect("an array is a sheet");
    assert_eq!((l.cols, l.rows, l.frames), (3, 2, 5));
    assert_eq!(l.kind, SheetKind::ArrayLayers);
    assert_eq!((img.width, img.height), (12, 8));
    // The sixth cell was never written, so it is transparent black rather than a stale layer.
    let (cx, cy, cell, sheet_w) = (2usize, 1usize, 4usize, 12usize);
    assert_eq!(rgba8(&img, cy * cell * sheet_w + cx * cell), [0, 0, 0, 0]);
}

/// A volume's slices live inside one array layer rather than in layers of their own, so they are
/// carved out of the level instead of indexed by layer.
#[test]
fn a_volume_tiles_its_depth_slices() {
    let mut dds = multi(DxgiFormat::BC1_UNorm, 4, 4, 1, false, Some(4), None);
    fill_blocks(&mut dds, &[0xf800, 0x07e0, 0x001f, 0xffe0]);
    let img = decode(&bytes(&dds), Some("dds"), &opts()).expect("volume decodes");

    let l = img.layout.expect("a volume is a sheet");
    assert_eq!((l.cols, l.rows, l.frames), (2, 2, 4));
    assert_eq!(l.kind, SheetKind::VolumeSlices);
    assert_eq!((img.width, img.height), (8, 8));
    assert_eq!(rgba8(&img, 0), [255, 0, 0, 255], "slice 0");
    assert_eq!(rgba8(&img, 4), [0, 255, 0, 255], "slice 1");
}

/// A volume's depth halves with each mip, so level 1 holds half as many slices and cannot tile
/// the same grid. Rather than show a level that is not a level of this sheet, the chain is
/// dropped and the viewer builds one from the composited level 0.
#[test]
fn a_volumes_chain_is_not_adopted() {
    let mut dds = multi(DxgiFormat::BC1_UNorm, 8, 8, 1, false, Some(4), Some(2));
    fill_blocks(&mut dds, &[0xf800]);
    let img = decode(&bytes(&dds), Some("dds"), &opts()).expect("mipped volume decodes");
    assert!(img.layout.is_some());
    assert!(img.source_mips.is_none());
}

/// A cubemap's own chain *is* adopted, because a power-of-two face tiles every level exactly:
/// `cols * (w >> n)` and `(cols * w) >> n` agree all the way down.
#[test]
fn a_cubemaps_chain_is_adopted_when_the_faces_tile_exactly() {
    let mut dds = multi(DxgiFormat::BC1_UNorm, 8, 8, 6, true, None, Some(4));
    fill_blocks(&mut dds, &[0xf800]);
    let img = decode(&bytes(&dds), Some("dds"), &opts()).expect("mipped cubemap decodes");

    assert_eq!((img.width, img.height), (24, 16));
    let mips = img.source_mips.expect("the faces tile every level");
    // The 24x16 sheet has five levels; level 1 is a 3x2 tiling of 4x4 faces.
    assert_eq!(mips[0].len(), 12 * 8 * 4);
    assert_eq!(mips[1].len(), 6 * 4 * 4);
}

/// An ordinary single-surface file is not a sheet, and must not pay the tiling copy or grow a
/// transport band it has no use for.
#[test]
fn a_single_surface_file_has_no_layout() {
    let file = dxgi(DxgiFormat::BC1_UNorm, 4, 4, &bc1_solid(0xf800));
    let img = decode(&file, Some("dds"), &opts()).expect("BC1 decodes");
    assert!(img.layout.is_none());
}

// --- alpha -----------------------------------------------------------------------------------

/// `DXT2` is `DXT3` with the colour channels already multiplied by alpha. Displaying that as-is
/// renders every semi-transparent area far too dark, so it is straightened back out — and the
/// otherwise identical `DXT3` must *not* be, which is what pins the fix to the right files.
#[test]
fn dxt2_premultiplied_alpha_is_straightened_and_dxt3_is_not() {
    // BC2: eight bytes of 4-bit alpha, then a BC1-style colour block. Alpha nibble 8 widens to
    // 0x88 = 136; the colour is red at 16/31, which bcdec widens to 132.
    let mut data = [0x88u8; 16];
    data[8..].copy_from_slice(&bc1_solid(16 << 11));

    let straight = decode(&d3d(D3DFormat::DXT3, 4, 4, &data), None, &opts()).expect("DXT3");
    assert_eq!(rgba8(&straight, 0), [132, 0, 0, 136]);

    let premul = decode(&d3d(D3DFormat::DXT2, 4, 4, &data), None, &opts()).expect("DXT2");
    // 132 * 255 / 136 = 247.5, and the rounding lands it on 248.
    assert_eq!(rgba8(&premul, 0), [248, 0, 0, 136]);
}

// --- routing and guards ------------------------------------------------------------------------

/// Routing is by magic bytes, so a mislabelled file still opens — and a `.dds` extension is
/// never what makes it work.
#[test]
fn dds_routes_by_magic_not_by_extension() {
    let file = dxgi(DxgiFormat::BC1_UNorm, 4, 4, &bc1_solid(0xf800));
    for hint in [None, Some("png"), Some("DDS"), Some("")] {
        let img = decode(&file, hint, &opts()).expect("routes by magic");
        assert_eq!(img.source_format, "DDS BC1", "with hint {hint:?}");
    }
}

/// A 124-byte DDS header laid out by hand, so a test can state dimensions and a format the
/// `ddsfile` writer would insist on allocating a buffer for.
fn raw_header(width: u32, height: u32, fourcc: Option<&[u8; 4]>) -> Vec<u8> {
    let mut b = Vec::with_capacity(128);
    b.extend_from_slice(b"DDS ");
    b.extend_from_slice(&124u32.to_le_bytes()); // header size
    b.extend_from_slice(&0x1007u32.to_le_bytes()); // CAPS | HEIGHT | WIDTH | PIXELFORMAT
    b.extend_from_slice(&height.to_le_bytes());
    b.extend_from_slice(&width.to_le_bytes());
    b.extend_from_slice(&0u32.to_le_bytes()); // pitch / linear size
    b.extend_from_slice(&0u32.to_le_bytes()); // depth
    b.extend_from_slice(&0u32.to_le_bytes()); // mip count
    b.extend_from_slice(&[0u8; 44]); // reserved1
                                     // pixel format
    b.extend_from_slice(&32u32.to_le_bytes()); // pixel format size
    match fourcc {
        Some(cc) => {
            b.extend_from_slice(&0x4u32.to_le_bytes()); // FOURCC
            b.extend_from_slice(cc);
            b.extend_from_slice(&[0u8; 20]); // bit count and the four masks
        }
        None => {
            b.extend_from_slice(&0x41u32.to_le_bytes()); // RGB | ALPHAPIXELS
            b.extend_from_slice(&[0u8; 4]); // no fourcc
            b.extend_from_slice(&32u32.to_le_bytes()); // bits per pixel
            b.extend_from_slice(&0x00ff_u32.to_le_bytes());
            b.extend_from_slice(&0xff00_u32.to_le_bytes());
            b.extend_from_slice(&0x00ff_0000_u32.to_le_bytes());
            b.extend_from_slice(&0xff00_0000_u32.to_le_bytes());
        }
    }
    b.extend_from_slice(&0x1000u32.to_le_bytes()); // caps: TEXTURE
    b.extend_from_slice(&[0u8; 16]); // caps2..4, reserved2
    assert_eq!(b.len(), 128);
    b
}

/// A header can claim a surface far larger than the file, and nothing downstream can catch a
/// failed allocation — it aborts the process rather than panicking. So the claim is refused
/// here, from the header's own numbers, before a byte is allocated.
#[test]
fn a_decode_bomb_header_is_refused_before_allocating() {
    // 100000 x 100000 clears the per-axis guard and blows the 4 GiB byte budget forty times over.
    let file = raw_header(100_000, 100_000, None);
    match decode(&file, Some("dds"), &opts()) {
        Err(DecodeError::TooLarge(m)) => assert!(m.contains("DDS"), "{m}"),
        other => panic!("expected TooLarge, got {other:?}"),
    }
}

/// A sheet multiplies the header's dimensions, so the guard runs again on the product.
///
/// A 16384² surface is a legal 1 GiB image on its own and passes the first check; six of them
/// tiled 3x2 is 49152x32768, still inside the per-axis limit but 6.4 GB of pixels — exactly the
/// multiplication a per-surface check alone would wave through.
#[test]
fn a_sheet_that_only_overflows_once_tiled_is_refused() {
    let mut file = raw_header(16_384, 16_384, None);
    // caps2 sits at byte 112: CUBEMAP plus all six face flags, which is how a legacy cubemap
    // announces itself. Built by hand because the `ddsfile` writer would try to allocate it.
    file[112..116].copy_from_slice(&(0x200u32 | 0xfc00).to_le_bytes());

    match decode(&file, Some("dds"), &opts()) {
        Err(DecodeError::TooLarge(m)) => assert!(m.contains("sheet"), "{m}"),
        other => panic!("expected TooLarge, got {other:?}"),
    }
}

/// A header whose surface data was truncated away is malformed, not a panic and not a buffer
/// of whatever happened to follow.
#[test]
fn a_truncated_surface_is_malformed() {
    let file = raw_header(64, 64, None); // header only: no pixel data at all
    match decode(&file, Some("dds"), &opts()) {
        Err(DecodeError::Malformed(m)) => assert!(m.contains("surface data"), "{m}"),
        other => panic!("expected Malformed, got {other:?}"),
    }
}

/// DDS carries roughly a hundred pixel formats and this decoder does not implement the video
/// and planar ones. Refusing them by name is the difference between a user knowing why their
/// file did not open and guessing.
#[test]
fn an_unsupported_format_names_itself() {
    let mut file = raw_header(4, 4, Some(b"YUY2"));
    file.extend_from_slice(&[0u8; 64]);
    match decode(&file, Some("dds"), &opts()) {
        Err(DecodeError::Malformed(m)) => {
            assert!(m.contains("unsupported DDS format"), "{m}");
            assert!(m.contains("YUY2"), "{m}");
        }
        other => panic!("expected Malformed, got {other:?}"),
    }
}
