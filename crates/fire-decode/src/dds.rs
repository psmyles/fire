//! DDS (DirectDraw Surface), decompressed on the CPU into the ordinary RGBA pipeline.
//!
//! DDS is a container, not a codec: a 124-byte header (plus a 20-byte DX10 extension on modern
//! files) followed by raw surface data in one of ~100 pixel formats. The two halves are split
//! here the same way. `ddsfile` parses the header, and `bcdec_rs` — a safe port of iOrange's
//! `bcdec` — decodes the block-compressed payloads. Both are pure Rust, so this backend needs no
//! vendored native tree and builds on every target the viewer does.
//!
//! **The payload is decompressed, not uploaded compressed.** sokol_gfx can take BC1-BC7 blocks
//! straight to the GPU, and that would be exact and cheap — but [`DecodedImage`] is an
//! *uncompressed RGBA canvas* by contract, and everything downstream reads it as one: the mip
//! builder, the downscale guard, the alpha scan, the flipbook grid detector. Teaching all of
//! them about block payloads to save tens of milliseconds on a decode that already runs off the
//! UI thread is a bad trade, so the blocks are expanded here and the rest of the viewer never
//! learns DDS exists.
//!
//! What the formats map to:
//!
//! | Source | Output |
//! |---|---|
//! | BC1/BC2/BC3/BC7 (and their sRGB spellings) | `Rgba8Unorm` |
//! | BC4 | `Rgba8Unorm`, the one channel replicated to grey |
//! | BC5 | `Rgba8Unorm` as `(r, g, 0, 1)` — the normal-map layout, shown as authored |
//! | BC6H | `Rgba16Float` — the HDR path, so exposure and tonemap apply |
//! | 16-bit and half-float channels | `Rgba16Unorm` / `Rgba16Float` |
//! | 32-bit float, and the packed floats (R11G11B10, RGB9E5) | `Rgba32Float` |
//! | everything else uncompressed | via the header's channel bit masks |
//!
//! BC6H is the first producer of [`PixelFormat::Rgba16Float`], which the renderer has always
//! understood but nothing decoded to.
//!
//! Signed block formats (`BC4_SNorm`, `BC5_SNorm`) are re-centred for display: `bcdec` hands
//! back the raw -127..127 range reinterpreted as bytes, which would show a normal map as noise,
//! so [`snorm_to_unorm`] maps it to 0..255 the way a sampler would.
//!
//! Only mip 0 of the first array layer is read. The file's own mip chain and its cubemap /
//! array / volume slices are located by [`Surfaces`] but not yet surfaced to the viewer.

use ddsfile::{Dds, DxgiFormat, FourCC, PixelFormatFlags};

use crate::{check_dims, DecodeError, DecodedImage, PixelFormat};

/// Half-precision 1.0, the alpha filled in for the alpha-less HDR formats.
const HALF_ONE: u16 = 0x3c00;

// --- format identification -----------------------------------------------------------------

/// A block-compressed payload. The `bool` is "the channels are signed".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Block {
    Bc1,
    Bc2,
    Bc3,
    Bc4(bool),
    Bc5(bool),
    Bc6h(bool),
    Bc7,
}

impl Block {
    /// Bytes per 4x4 block. BC1 and BC4 carry one endpoint pair; the rest carry two.
    fn block_bytes(self) -> usize {
        match self {
            Block::Bc1 | Block::Bc4(_) => 8,
            _ => 16,
        }
    }
}

/// An uncompressed payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Plain {
    /// Integer channels carved out of a `bytes`-wide little-endian unit by the header's R/G/B/A
    /// bit masks. This one arm covers every legacy D3D9 layout — `A8R8G8B8`, `X8B8G8R8`,
    /// `R5G6B5`, `A2R10G10B10`, `L8`, `A8` — plus whatever masks an exporter invents, because it
    /// reads the masks rather than matching a table of known ones. `luminance` replicates the red
    /// channel across green and blue (a grey or single-channel source).
    Masked {
        bytes: u8,
        masks: [u32; 4],
        luminance: bool,
    },
    /// `n` unsigned 16-bit channels in R, G, B, A order.
    U16(u8),
    /// `n` half-float channels.
    F16(u8),
    /// `n` 32-bit float channels.
    F32(u8),
    /// Three unsigned floats packed 11/11/10 into one 32-bit word.
    R11G11B10,
    /// Three 9-bit mantissas sharing one 5-bit exponent, packed into 32 bits.
    R9G9B9E5,
}

impl Plain {
    /// Bytes per texel in the file.
    fn texel_bytes(self) -> usize {
        match self {
            Plain::Masked { bytes, .. } => bytes as usize,
            Plain::U16(n) | Plain::F16(n) => 2 * n as usize,
            Plain::F32(n) => 4 * n as usize,
            Plain::R11G11B10 | Plain::R9G9B9E5 => 4,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Codec {
    Block(Block),
    Plain(Plain),
}

/// How the file stores its texels, and what they become.
#[derive(Debug, Clone, Copy)]
struct Format {
    codec: Codec,
    /// The [`DecodedImage`] layout this decodes to.
    out: PixelFormat,
    /// Bits per channel of the source, for the status bar.
    bit_depth: u8,
    /// The source's meaningful channel count. Two-channel sources (BC5, R16G16) report 3: they
    /// are displayed as RGB with a zero blue lane, and calling them "Gray+A" would be a lie.
    channels: u8,
    /// The status-bar name for the encoding.
    label: &'static str,
}

impl Format {
    fn block(b: Block, out: PixelFormat, bit_depth: u8, channels: u8, label: &'static str) -> Self {
        Format {
            codec: Codec::Block(b),
            out,
            bit_depth,
            channels,
            label,
        }
    }

    fn plain(p: Plain, out: PixelFormat, bit_depth: u8, channels: u8, label: &'static str) -> Self {
        Format {
            codec: Codec::Plain(p),
            out,
            bit_depth,
            channels,
            label,
        }
    }

    /// Bytes one `w`x`h`x`d` surface occupies in the file, or `None` if that overflows.
    fn level_bytes(&self, w: u32, h: u32, d: u32) -> Option<usize> {
        let (w, h, d) = (w as usize, h as usize, d as usize);
        match self.codec {
            // Block formats round up to whole 4x4 blocks, so a 5x3 level still costs 2x1 blocks.
            Codec::Block(b) => w
                .div_ceil(4)
                .checked_mul(h.div_ceil(4))?
                .checked_mul(b.block_bytes())?
                .checked_mul(d),
            Codec::Plain(p) => w
                .checked_mul(h)?
                .checked_mul(p.texel_bytes())?
                .checked_mul(d),
        }
    }
}

/// Identify the payload from the header. `None` for a format we have no decoder for, which the
/// caller turns into a `Malformed` naming it.
fn identify(dds: &Dds) -> Option<Format> {
    let spf = &dds.header.spf;
    if spf.flags.contains(PixelFormatFlags::FOURCC) {
        let cc = spf.fourcc.as_ref()?.0;
        if cc == FourCC::DX10 {
            return from_dxgi(dds.header10.as_ref()?.dxgi_format);
        }
        return from_fourcc(cc);
    }
    from_masks(spf)
}

/// The DX10 header's `DxgiFormat`. Typeless variants decode as their unsigned-normalized
/// spelling: a viewer has to show *something*, and that is what every other tool picks.
fn from_dxgi(f: DxgiFormat) -> Option<Format> {
    use DxgiFormat as D;
    use PixelFormat::{Rgba16Float, Rgba16Unorm, Rgba32Float, Rgba8Unorm};
    Some(match f {
        D::BC1_Typeless | D::BC1_UNorm | D::BC1_UNorm_sRGB => {
            Format::block(Block::Bc1, Rgba8Unorm, 8, 4, "DDS BC1")
        }
        D::BC2_Typeless | D::BC2_UNorm | D::BC2_UNorm_sRGB => {
            Format::block(Block::Bc2, Rgba8Unorm, 8, 4, "DDS BC2")
        }
        D::BC3_Typeless | D::BC3_UNorm | D::BC3_UNorm_sRGB => {
            Format::block(Block::Bc3, Rgba8Unorm, 8, 4, "DDS BC3")
        }
        D::BC4_Typeless | D::BC4_UNorm => {
            Format::block(Block::Bc4(false), Rgba8Unorm, 8, 1, "DDS BC4")
        }
        D::BC4_SNorm => Format::block(Block::Bc4(true), Rgba8Unorm, 8, 1, "DDS BC4"),
        D::BC5_Typeless | D::BC5_UNorm => {
            Format::block(Block::Bc5(false), Rgba8Unorm, 8, 3, "DDS BC5")
        }
        D::BC5_SNorm => Format::block(Block::Bc5(true), Rgba8Unorm, 8, 3, "DDS BC5"),
        D::BC6H_Typeless | D::BC6H_UF16 => {
            Format::block(Block::Bc6h(false), Rgba16Float, 16, 3, "DDS BC6H")
        }
        D::BC6H_SF16 => Format::block(Block::Bc6h(true), Rgba16Float, 16, 3, "DDS BC6H"),
        D::BC7_Typeless | D::BC7_UNorm | D::BC7_UNorm_sRGB => {
            Format::block(Block::Bc7, Rgba8Unorm, 8, 4, "DDS BC7")
        }

        // 32-bit float.
        D::R32G32B32A32_Typeless | D::R32G32B32A32_Float => {
            Format::plain(Plain::F32(4), Rgba32Float, 32, 4, "DDS RGBA32F")
        }
        D::R32G32B32_Typeless | D::R32G32B32_Float => {
            Format::plain(Plain::F32(3), Rgba32Float, 32, 3, "DDS RGB32F")
        }
        D::R32G32_Typeless | D::R32G32_Float => {
            Format::plain(Plain::F32(2), Rgba32Float, 32, 3, "DDS RG32F")
        }
        D::R32_Typeless | D::R32_Float | D::D32_Float => {
            Format::plain(Plain::F32(1), Rgba32Float, 32, 1, "DDS R32F")
        }

        // Half float.
        D::R16G16B16A16_Typeless | D::R16G16B16A16_Float => {
            Format::plain(Plain::F16(4), Rgba16Float, 16, 4, "DDS RGBA16F")
        }
        D::R16G16_Float => Format::plain(Plain::F16(2), Rgba16Float, 16, 3, "DDS RG16F"),
        D::R16_Float => Format::plain(Plain::F16(1), Rgba16Float, 16, 1, "DDS R16F"),

        // 16-bit unsigned normalized.
        D::R16G16B16A16_UNorm => Format::plain(Plain::U16(4), Rgba16Unorm, 16, 4, "DDS RGBA16"),
        D::R16G16_UNorm => Format::plain(Plain::U16(2), Rgba16Unorm, 16, 3, "DDS RG16"),
        D::R16_UNorm | D::D16_UNorm => Format::plain(Plain::U16(1), Rgba16Unorm, 16, 1, "DDS R16"),

        // Packed floats.
        D::R11G11B10_Float => Format::plain(Plain::R11G11B10, Rgba32Float, 11, 3, "DDS R11G11B10F"),
        D::R9G9B9E5_SharedExp => Format::plain(Plain::R9G9B9E5, Rgba32Float, 9, 3, "DDS RGB9E5"),

        // Integer channels. The DX10 header names a channel order rather than bit masks, so the
        // masks are spelled out here and share the one mask-driven expander.
        D::R8G8B8A8_Typeless | D::R8G8B8A8_UNorm | D::R8G8B8A8_UNorm_sRGB => masked(
            4,
            [0xff, 0xff00, 0xff_0000, 0xff00_0000],
            false,
            "DDS RGBA8",
        ),
        D::B8G8R8A8_Typeless | D::B8G8R8A8_UNorm | D::B8G8R8A8_UNorm_sRGB => masked(
            4,
            [0xff_0000, 0xff00, 0xff, 0xff00_0000],
            false,
            "DDS BGRA8",
        ),
        D::B8G8R8X8_Typeless | D::B8G8R8X8_UNorm | D::B8G8R8X8_UNorm_sRGB => {
            masked(4, [0xff_0000, 0xff00, 0xff, 0], false, "DDS BGRX8")
        }
        D::R10G10B10A2_Typeless | D::R10G10B10A2_UNorm => masked(
            4,
            [0x3ff, 0xf_fc00, 0x3ff0_0000, 0xc000_0000],
            false,
            "DDS RGB10A2",
        ),
        D::R8G8_Typeless | D::R8G8_UNorm => masked(2, [0xff, 0xff00, 0, 0], false, "DDS RG8"),
        D::R8_Typeless | D::R8_UNorm => masked(1, [0xff, 0, 0, 0], true, "DDS R8"),
        D::A8_UNorm => masked(1, [0, 0, 0, 0xff], true, "DDS A8"),
        D::B5G6R5_UNorm => masked(2, [0xf800, 0x7e0, 0x1f, 0], false, "DDS B5G6R5"),
        D::B5G5R5A1_UNorm => masked(2, [0x7c00, 0x3e0, 0x1f, 0x8000], false, "DDS B5G5R5A1"),
        D::B4G4R4A4_UNorm => masked(2, [0xf00, 0xf0, 0xf, 0xf000], false, "DDS B4G4R4A4"),

        _ => return None,
    })
}

/// A legacy FourCC. `DXT2`/`DXT4` are `DXT3`/`DXT5` with premultiplied alpha — the same blocks,
/// straightened afterwards by [`decode`] — and the `ATI1`/`ATI2`/`BC4?`/`BC5?`/`3DC?` family is
/// how BC4 and BC5 were spelled before the DX10 header existed.
fn from_fourcc(cc: u32) -> Option<Format> {
    use PixelFormat::{Rgba16Float, Rgba16Unorm, Rgba32Float, Rgba8Unorm};
    const fn code(s: &[u8; 4]) -> u32 {
        (s[0] as u32) | ((s[1] as u32) << 8) | ((s[2] as u32) << 16) | ((s[3] as u32) << 24)
    }
    Some(match cc {
        FourCC::DXT1 => Format::block(Block::Bc1, Rgba8Unorm, 8, 4, "DDS BC1"),
        FourCC::DXT2 | FourCC::DXT3 => Format::block(Block::Bc2, Rgba8Unorm, 8, 4, "DDS BC2"),
        FourCC::DXT4 | FourCC::DXT5 => Format::block(Block::Bc3, Rgba8Unorm, 8, 4, "DDS BC3"),
        FourCC::ATI1 => Format::block(Block::Bc4(false), Rgba8Unorm, 8, 1, "DDS BC4"),
        FourCC::ATI2 => Format::block(Block::Bc5(false), Rgba8Unorm, 8, 3, "DDS BC5"),
        FourCC::A16B16G16R16 => Format::plain(Plain::U16(4), Rgba16Unorm, 16, 4, "DDS RGBA16"),
        FourCC::R16F => Format::plain(Plain::F16(1), Rgba16Float, 16, 1, "DDS R16F"),
        FourCC::G16R16F => Format::plain(Plain::F16(2), Rgba16Float, 16, 3, "DDS RG16F"),
        FourCC::A16B16G16R16F => Format::plain(Plain::F16(4), Rgba16Float, 16, 4, "DDS RGBA16F"),
        FourCC::R32F => Format::plain(Plain::F32(1), Rgba32Float, 32, 1, "DDS R32F"),
        FourCC::G32R32F => Format::plain(Plain::F32(2), Rgba32Float, 32, 3, "DDS RG32F"),
        FourCC::A32B32G32R32F => Format::plain(Plain::F32(4), Rgba32Float, 32, 4, "DDS RGBA32F"),
        c if c == code(b"BC4U") || c == code(b"3DC1") => {
            Format::block(Block::Bc4(false), Rgba8Unorm, 8, 1, "DDS BC4")
        }
        c if c == code(b"BC4S") => Format::block(Block::Bc4(true), Rgba8Unorm, 8, 1, "DDS BC4"),
        c if c == code(b"BC5U") || c == code(b"3DC2") => {
            Format::block(Block::Bc5(false), Rgba8Unorm, 8, 3, "DDS BC5")
        }
        c if c == code(b"BC5S") => Format::block(Block::Bc5(true), Rgba8Unorm, 8, 3, "DDS BC5"),
        _ => return None,
    })
}

/// Build a masked format, choosing the output depth from the widest channel: a 10- or 16-bit
/// lane keeps its precision in `Rgba16Unorm` rather than being crushed to 8.
fn masked(bytes: u8, masks: [u32; 4], luminance: bool, label: &'static str) -> Format {
    let widest = masks.iter().copied().map(mask_width).max().unwrap_or(0);
    let out = if widest > 8 {
        PixelFormat::Rgba16Unorm
    } else {
        PixelFormat::Rgba8Unorm
    };
    // One colour lane (or an explicit luminance flag) means grey: replicate red across green and
    // blue rather than showing an R8 height map as a red one.
    let colour = u8::from(masks[0] != 0) + u8::from(masks[1] != 0) + u8::from(masks[2] != 0);
    let grey = luminance || colour <= 1;
    let has_alpha = masks[3] != 0;
    let channels = match (grey, has_alpha) {
        (true, true) => 2,
        (true, false) => 1,
        (false, true) => 4,
        (false, false) => 3,
    };
    Format::plain(
        Plain::Masked {
            bytes,
            masks,
            luminance: grey,
        },
        out,
        widest.max(1) as u8,
        channels,
        label,
    )
}

/// Bits in a (contiguous) channel mask.
fn mask_width(m: u32) -> u32 {
    if m == 0 {
        0
    } else {
        (m >> m.trailing_zeros()).count_ones()
    }
}

/// A header that describes its channels with bit masks rather than a FourCC.
fn from_masks(spf: &ddsfile::PixelFormat) -> Option<Format> {
    let bits = spf.rgb_bit_count?;
    if !matches!(bits, 8 | 16 | 24 | 32) {
        return None;
    }
    let masks = [
        spf.r_bit_mask.unwrap_or(0),
        spf.g_bit_mask.unwrap_or(0),
        spf.b_bit_mask.unwrap_or(0),
        spf.a_bit_mask.unwrap_or(0),
    ];
    let widest = masks.iter().copied().map(mask_width).max()?;
    // A mask wider than the unit it sits in, or none at all, is not a layout we can read.
    if widest == 0
        || widest > 16
        || masks
            .iter()
            .any(|&m| m != 0 && m.leading_zeros() < 32 - bits)
    {
        return None;
    }
    let luminance = spf.flags.contains(PixelFormatFlags::LUMINANCE);
    Some(masked((bits / 8) as u8, masks, luminance, "DDS"))
}

/// What the header says the format is, for an error message. Falls back to the raw FourCC, then
/// to the bit masks, so an unsupported file says which format it was rather than just "no".
fn describe(dds: &Dds) -> String {
    if let Some(h10) = dds.header10.as_ref() {
        return format!("{:?}", h10.dxgi_format);
    }
    match dds.header.spf.fourcc.as_ref().map(|f| f.0) {
        Some(cc) if cc != FourCC::NONE => {
            let b = cc.to_le_bytes();
            if b.iter().all(u8::is_ascii_graphic) {
                format!("FourCC {}", String::from_utf8_lossy(&b))
            } else {
                format!("FourCC {cc:#x}")
            }
        }
        _ => format!(
            "{}-bit masks R={:#x} G={:#x} B={:#x} A={:#x}",
            dds.header.spf.rgb_bit_count.unwrap_or(0),
            dds.header.spf.r_bit_mask.unwrap_or(0),
            dds.header.spf.g_bit_mask.unwrap_or(0),
            dds.header.spf.b_bit_mask.unwrap_or(0),
            dds.header.spf.a_bit_mask.unwrap_or(0),
        ),
    }
}

// --- surface layout ------------------------------------------------------------------------

/// Where every surface sits in the file's data blob.
///
/// A DDS packs its surfaces tightly, array layer by array layer (a cubemap is six layers, in
/// `+X -X +Y -Y +Z -Z` order), and within a layer mip level by mip level, largest first. Each
/// level halves both axes, flooring but never going below 1 — the same rule the viewer's own mip
/// builder uses, which is what will let the file's chain be adopted verbatim.
struct Surfaces {
    levels: u32,
    layers: u32,
    /// Byte size of each mip level of one layer, level 0 first.
    level_sizes: Vec<usize>,
    /// Bytes from the start of one layer to the start of the next.
    layer_stride: usize,
}

impl Surfaces {
    fn new(dds: &Dds, fmt: &Format, data_len: usize) -> Result<Surfaces, DecodeError> {
        let (w, h) = (dds.header.width, dds.header.height);
        let depth = dds.get_depth().max(1);
        // A header may claim more levels than the dimensions can have, and a corrupt one may
        // claim billions. Either way the chain stops where the halving reaches 1x1.
        let max_levels = 32 - w.max(h).max(1).leading_zeros();
        let levels = dds.get_num_mipmap_levels().clamp(1, max_levels);

        let mut level_sizes = Vec::with_capacity(levels as usize);
        let mut layer_stride = 0usize;
        let (mut lw, mut lh, mut ld) = (w, h, depth);
        for _ in 0..levels {
            let n = fmt.level_bytes(lw, lh, ld).ok_or_else(|| {
                DecodeError::TooLarge(format!("DDS {w}x{h} mip chain overflows a usize"))
            })?;
            layer_stride = layer_stride.checked_add(n).ok_or_else(|| {
                DecodeError::TooLarge(format!("DDS {w}x{h} mip chain overflows a usize"))
            })?;
            level_sizes.push(n);
            lw = (lw / 2).max(1);
            lh = (lh / 2).max(1);
            ld = (ld / 2).max(1);
        }

        // A cubemap header always claims six faces even when only some were written, and a DX10
        // `array_size` is attacker-controlled — so the layer count is what the data can actually
        // back, never what the header asks for.
        let declared = dds.get_num_array_layers().max(1);
        let backed = data_len
            .checked_div(layer_stride)
            .unwrap_or(0)
            .min(u32::MAX as usize) as u32;
        let layers = declared.min(backed);
        if layers == 0 {
            return Err(DecodeError::Malformed(format!(
                "DDS holds {data_len} bytes of surface data, short of the {layer_stride} its \
                 {w}x{h} header describes"
            )));
        }
        Ok(Surfaces {
            levels,
            layers,
            level_sizes,
            layer_stride,
        })
    }

    /// Byte range of one surface within the data blob.
    fn range(&self, layer: u32, level: u32) -> Option<std::ops::Range<usize>> {
        if layer >= self.layers || level >= self.levels {
            return None;
        }
        let start = (layer as usize).checked_mul(self.layer_stride)?
            + self.level_sizes[..level as usize].iter().sum::<usize>();
        Some(start..start + self.level_sizes[level as usize])
    }
}

// --- decoding ------------------------------------------------------------------------------

/// Decode a DDS from memory: mip 0 of the first array layer, normalized to RGBA.
pub fn decode(bytes: &[u8]) -> Result<DecodedImage, DecodeError> {
    let dds = Dds::read(bytes).map_err(|e| DecodeError::Malformed(format!("DDS: {e}")))?;
    let fmt = identify(&dds).ok_or_else(|| {
        DecodeError::Malformed(format!("unsupported DDS format: {}", describe(&dds)))
    })?;

    let (width, height) = (dds.header.width, dds.header.height);
    if width == 0 || height == 0 {
        return Err(DecodeError::Malformed(format!(
            "DDS declares a {width}x{height} surface"
        )));
    }
    // Before anything is allocated from the header's numbers.
    check_dims(
        width as usize,
        height as usize,
        fmt.out.bytes_per_pixel(),
        "DDS",
    )?;

    let surfaces = Surfaces::new(&dds, &fmt, dds.data.len())?;
    let range = surfaces
        .range(0, 0)
        .ok_or_else(|| DecodeError::Malformed("DDS has no surfaces".into()))?;
    let src = dds
        .data
        .get(range)
        .ok_or_else(|| DecodeError::Malformed("DDS surface data is truncated".into()))?;

    let mut pixels = decode_surface(&fmt, src, width, height);

    // `DXT2`/`DXT4`, and any DX10 header that says so, store colour already multiplied by alpha.
    // Displaying that directly renders every semi-transparent area about twice too dark.
    if premultiplied(&dds) {
        straighten(&mut pixels, fmt.out);
    }

    Ok(DecodedImage {
        pixels,
        width,
        height,
        format: fmt.out,
        bit_depth: fmt.bit_depth,
        channels: fmt.channels,
        icc: None,
        source_format: fmt.label,
        alpha_opaque: false, // set by `decode` after the final buffer is built
        downscaled_from: None,
        animation: None,
    })
}

/// Whether the file says its colour channels are already multiplied by alpha.
fn premultiplied(dds: &Dds) -> bool {
    if let Some(h10) = dds.header10.as_ref() {
        return h10.alpha_mode == ddsfile::AlphaMode::PreMultiplied;
    }
    matches!(
        dds.header.spf.fourcc.as_ref().map(|f| f.0),
        Some(FourCC::DXT2 | FourCC::DXT4)
    )
}

/// Decode one surface into a fresh RGBA buffer. `src` is exactly that surface's bytes.
fn decode_surface(fmt: &Format, src: &[u8], w: u32, h: u32) -> Vec<u8> {
    let bpp = fmt.out.bytes_per_pixel();
    let row = w as usize * bpp;
    let mut out = vec![0u8; row * h as usize];
    match fmt.codec {
        Codec::Block(b) => decode_blocks(b, src, w, h, &mut out, row, bpp),
        Codec::Plain(p) => decode_plain(p, src, fmt.out, &mut out),
    }
    out
}

/// Surfaces with at least this many texels are expanded on several threads. Below it the
/// hand-off costs more than the work — the same threshold the mip builder uses.
const PARALLEL_MIN_TEXELS: usize = 256 * 256;

/// Expand block-compressed `src` into `out`, one 4x4 block at a time.
fn decode_blocks(b: Block, src: &[u8], w: u32, h: u32, out: &mut [u8], row: usize, bpp: usize) {
    let bw = (w as usize).div_ceil(4);
    let bsz = b.block_bytes();
    let w = w as usize;

    // One unit of work is a block row: four output rows, fewer at the bottom edge.
    let block_row = |by: usize, dst: &mut [u8]| {
        let rows = dst.len() / row;
        for bx in 0..bw {
            let off = (by * bw + bx) * bsz;
            let Some(blk) = src.get(off..off + bsz) else {
                return;
            };
            let x0 = bx * 4;
            emit_block(b, blk, dst, row, x0 * bpp, (w - x0).min(4), rows);
        }
    };

    let block_rows = (h as usize).div_ceil(4);
    let threads = if w * h as usize >= PARALLEL_MIN_TEXELS {
        std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .clamp(1, 8)
            .min(block_rows.max(1))
    } else {
        1
    };
    if threads <= 1 {
        for (by, dst) in out.chunks_mut(4 * row).enumerate() {
            block_row(by, dst);
        }
    } else {
        let rows_per = block_rows.div_ceil(threads);
        std::thread::scope(|scope| {
            for (i, chunk) in out.chunks_mut(rows_per * 4 * row).enumerate() {
                let block_row = &block_row;
                scope.spawn(move || {
                    for (j, dst) in chunk.chunks_mut(4 * row).enumerate() {
                        block_row(i * rows_per + j, dst);
                    }
                });
            }
        });
    }
}

/// Decode one 4x4 block and scatter its in-bounds texels into `dst`, a band of up to four output
/// rows. Each codec decodes into a scratch block of its own natural layout first, which is what
/// keeps the edge blocks of a non-multiple-of-4 surface from writing out of bounds.
fn emit_block(
    b: Block,
    blk: &[u8],
    dst: &mut [u8],
    row: usize,
    xoff: usize,
    cols: usize,
    rows: usize,
) {
    match b {
        Block::Bc1 | Block::Bc2 | Block::Bc3 | Block::Bc7 => {
            let f: fn(&[u8], &mut [u8], usize) = match b {
                Block::Bc1 => bcdec_rs::bc1,
                Block::Bc2 => bcdec_rs::bc2,
                Block::Bc3 => bcdec_rs::bc3,
                _ => bcdec_rs::bc7,
            };
            let mut s = [0u8; 4 * 4 * 4];
            f(blk, &mut s, 4 * 4);
            for y in 0..rows {
                dst[y * row + xoff..y * row + xoff + cols * 4]
                    .copy_from_slice(&s[y * 16..y * 16 + cols * 4]);
            }
        }
        Block::Bc4(signed) => {
            let mut s = [0u8; 4 * 4];
            bcdec_rs::bc4(blk, &mut s, 4, signed);
            for y in 0..rows {
                for x in 0..cols {
                    let v = snorm_to_unorm(s[y * 4 + x], signed);
                    dst[y * row + xoff + x * 4..][..4].copy_from_slice(&[v, v, v, 255]);
                }
            }
        }
        Block::Bc5(signed) => {
            let mut s = [0u8; 4 * 4 * 2];
            bcdec_rs::bc5(blk, &mut s, 4 * 2, signed);
            for y in 0..rows {
                for x in 0..cols {
                    let r = snorm_to_unorm(s[y * 8 + x * 2], signed);
                    let g = snorm_to_unorm(s[y * 8 + x * 2 + 1], signed);
                    dst[y * row + xoff + x * 4..][..4].copy_from_slice(&[r, g, 0, 255]);
                }
            }
        }
        Block::Bc6h(signed) => {
            let mut s = [0u16; 4 * 4 * 3];
            bcdec_rs::bc6h_half(blk, &mut s, 4 * 3, signed);
            for y in 0..rows {
                for x in 0..cols {
                    let p = y * 12 + x * 3;
                    let d = &mut dst[y * row + xoff + x * 8..][..8];
                    d[0..2].copy_from_slice(&s[p].to_ne_bytes());
                    d[2..4].copy_from_slice(&s[p + 1].to_ne_bytes());
                    d[4..6].copy_from_slice(&s[p + 2].to_ne_bytes());
                    d[6..8].copy_from_slice(&HALF_ONE.to_ne_bytes());
                }
            }
        }
    }
}

/// Re-centre a signed block channel for display. `bcdec` hands back the -127..127 range
/// reinterpreted as a byte, so a signed normal map would otherwise show as noise; this is the
/// mapping a sampler applies, with 0 landing on mid-grey.
fn snorm_to_unorm(v: u8, signed: bool) -> u8 {
    if !signed {
        return v;
    }
    let x = i32::from((v as i8).max(-127)) + 127; // 0..254
    ((x * 255 + 127) / 254) as u8
}

/// Expand an uncompressed surface into `out`.
fn decode_plain(p: Plain, src: &[u8], out_format: PixelFormat, out: &mut [u8]) {
    let bpp = out_format.bytes_per_pixel();
    let texels = out.len() / bpp;
    let stride = p.texel_bytes();
    for i in 0..texels {
        let Some(unit) = src.get(i * stride..i * stride + stride) else {
            return;
        };
        let d = &mut out[i * bpp..i * bpp + bpp];
        match p {
            Plain::Masked {
                masks, luminance, ..
            } => {
                let word = read_le(unit);
                let to = if out_format == PixelFormat::Rgba16Unorm {
                    0xffff
                } else {
                    0xff
                };
                let r = channel(word, masks[0], to);
                let (g, b) = if luminance {
                    (r, r)
                } else {
                    (channel(word, masks[1], to), channel(word, masks[2], to))
                };
                let a = if masks[3] == 0 {
                    to
                } else {
                    channel(word, masks[3], to)
                };
                if to == 0xffff {
                    store_u16(d, [r as u16, g as u16, b as u16, a as u16]);
                } else {
                    d.copy_from_slice(&[r as u8, g as u8, b as u8, a as u8]);
                }
            }
            Plain::U16(n) => {
                store_u16(
                    d,
                    spread(n, u16::MAX, 0, |c| {
                        u16::from_le_bytes([unit[c * 2], unit[c * 2 + 1]])
                    }),
                );
            }
            Plain::F16(n) => {
                store_u16(
                    d,
                    spread(n, HALF_ONE, 0, |c| {
                        u16::from_le_bytes([unit[c * 2], unit[c * 2 + 1]])
                    }),
                );
            }
            Plain::F32(n) => {
                store_f32(
                    d,
                    spread(n, 1.0, 0.0, |c| {
                        f32::from_le_bytes([
                            unit[c * 4],
                            unit[c * 4 + 1],
                            unit[c * 4 + 2],
                            unit[c * 4 + 3],
                        ])
                    }),
                );
            }
            Plain::R11G11B10 => {
                let w = read_le(unit);
                store_f32(
                    d,
                    [
                        packed_float_to_f32(w & 0x7ff, 6),
                        packed_float_to_f32((w >> 11) & 0x7ff, 6),
                        packed_float_to_f32((w >> 22) & 0x3ff, 5),
                        1.0,
                    ],
                );
            }
            Plain::R9G9B9E5 => {
                let w = read_le(unit);
                // A 5-bit exponent biased by 15, shared by three 9-bit fractional mantissas.
                let scale = 2f32.powi(((w >> 27) & 0x1f) as i32 - 15 - 9);
                store_f32(
                    d,
                    [
                        (w & 0x1ff) as f32 * scale,
                        ((w >> 9) & 0x1ff) as f32 * scale,
                        ((w >> 18) & 0x1ff) as f32 * scale,
                        1.0,
                    ],
                );
            }
        }
    }
}

/// Widen `n` source channels to RGBA, replicating a single channel across the colour lanes (a
/// grey source) and zeroing the blue lane of a two-channel one.
fn spread<T: Copy>(n: u8, opaque: T, zero: T, v: impl Fn(usize) -> T) -> [T; 4] {
    match n {
        1 => [v(0), v(0), v(0), opaque],
        2 => [v(0), v(1), zero, opaque],
        3 => [v(0), v(1), v(2), opaque],
        _ => [v(0), v(1), v(2), v(3)],
    }
}

/// A 1-, 2-, 3- or 4-byte little-endian unit as a `u32`.
fn read_le(unit: &[u8]) -> u32 {
    let mut w = 0u32;
    for (i, b) in unit.iter().take(4).enumerate() {
        w |= u32::from(*b) << (8 * i);
    }
    w
}

/// Pull one masked channel out of `word` and rescale it to `0..=to`. Bit-exact for a mask as
/// wide as the target (8 or 16 bits) and correctly rounded for every other width.
fn channel(word: u32, mask: u32, to: u32) -> u32 {
    if mask == 0 {
        return 0;
    }
    let v = u64::from((word & mask) >> mask.trailing_zeros());
    let max = ((1u64 << mask_width(mask)) - 1).max(1);
    ((v * u64::from(to) + max / 2) / max) as u32
}

fn store_u16(d: &mut [u8], v: [u16; 4]) {
    for (i, x) in v.iter().enumerate() {
        d[i * 2..i * 2 + 2].copy_from_slice(&x.to_ne_bytes());
    }
}

fn store_f32(d: &mut [u8], v: [f32; 4]) {
    for (i, x) in v.iter().enumerate() {
        d[i * 4..i * 4 + 4].copy_from_slice(&x.to_ne_bytes());
    }
}

/// One unsigned packed float — the 11- and 10-bit lanes of `R11G11B10_Float` — with
/// `mantissa_bits` of mantissa and a 5-bit exponent biased by 15. No sign bit.
fn packed_float_to_f32(v: u32, mantissa_bits: u32) -> f32 {
    let max = (1u32 << mantissa_bits) - 1;
    let mantissa = v & max;
    let exponent = v >> mantissa_bits;
    let frac = mantissa as f32 / (max + 1) as f32;
    if exponent == 0 {
        // Denormal: no implicit leading 1.
        frac * 2f32.powi(-14)
    } else if exponent == 0x1f {
        if mantissa == 0 {
            f32::INFINITY
        } else {
            f32::NAN
        }
    } else {
        (1.0 + frac) * 2f32.powi(exponent as i32 - 15)
    }
}

/// Divide the colour channels back out of a premultiplied buffer.
fn straighten(pixels: &mut [u8], format: PixelFormat) {
    match format {
        PixelFormat::Rgba8Unorm => {
            for px in pixels.as_chunks_mut::<4>().0 {
                let a = u32::from(px[3]);
                if a == 0 || a == 255 {
                    continue;
                }
                for c in &mut px[..3] {
                    *c = ((u32::from(*c) * 255 + a / 2) / a).min(255) as u8;
                }
            }
        }
        PixelFormat::Rgba16Unorm => {
            for px in pixels.as_chunks_mut::<8>().0 {
                let a = u64::from(u16::from_ne_bytes([px[6], px[7]]));
                if a == 0 || a == 65535 {
                    continue;
                }
                for c in 0..3 {
                    let v = u64::from(u16::from_ne_bytes([px[c * 2], px[c * 2 + 1]]));
                    let s = ((v * 65535 + a / 2) / a).min(65535) as u16;
                    px[c * 2..c * 2 + 2].copy_from_slice(&s.to_ne_bytes());
                }
            }
        }
        PixelFormat::Rgba32Float => {
            for px in pixels.as_chunks_mut::<16>().0 {
                let a = f32::from_ne_bytes([px[12], px[13], px[14], px[15]]);
                if a <= 0.0 || a == 1.0 {
                    continue;
                }
                for c in 0..3 {
                    let b: [u8; 4] = px[c * 4..c * 4 + 4].try_into().unwrap();
                    let s = f32::from_ne_bytes(b) / a;
                    px[c * 4..c * 4 + 4].copy_from_slice(&s.to_ne_bytes());
                }
            }
        }
        // The only half-float source is BC6H, which has no alpha channel to have multiplied by.
        PixelFormat::Rgba16Float => {}
    }
}
