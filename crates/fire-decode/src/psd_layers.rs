//! PSD layers, one at a time: parse the layer and mask section, then decode any single layer on
//! demand into its own RGBA buffer.
//!
//! The merged composite still comes from psd_sdk ([`crate::decode`]); this module only exists for
//! layer stepping. It is pure Rust on purpose. psd_sdk's own layer path (`ParseLayerMaskSection` /
//! `ExtractLayer`) trusts the file far more than its composite path does — unchecked rectangles,
//! 32-bit size arithmetic, assertions on malformed input, and an `ExtractLayer` that mutates the
//! shared layer and cannot run twice — and a crash there takes every window with it, because Fire
//! is one process. Here every offset and size is checked against the bytes, a malformed layer is
//! a [`LayerError`], and a [`PsdLayers`] is immutable after [`PsdLayers::open`], so any number of
//! workers can decode different (or the same) layers at once.
//!
//! What a decoded layer is: the layer's own pixels — colour, its transparency channel, and its
//! layer and vector masks multiplied into alpha — over transparency, cropped to the part of its
//! bounds that lies on the canvas. Opacity, blend mode, clipping and layer styles are *not*
//! applied; [`LayerInfo`] carries them so the viewer can say so. Colour conversion (CMYK, Lab,
//! greyscale) follows `wrapper.cpp`'s composite path sample for sample, so a layer and the
//! composite of a one-layer document agree.
//!
//! Format notes this module relies on (Adobe's "Photoshop File Formats" specification):
//! records are stored bottom-most first; a group is a folder record (`lsct` 1 or 2) *above* its
//! children and a section-divider record (`lsct` 3) below them; channel image data follows all
//! the records, in record order; 16- and 32-bit documents keep their layers in the `Lr16`/`Lr32`
//! global block rather than the plain layer info.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::{check_dims, DecodeError, DecodeOptions, DecodedImage, PixelFormat};

const MODE_GRAYSCALE: u16 = 1;
const MODE_RGB: u16 = 3;
const MODE_CMYK: u16 = 4;
const MODE_DUOTONE: u16 = 8;
const MODE_LAB: u16 = 9;

/// Same cap `psd-sdk-sys` puts on the composite's profile.
const MAX_ICC_LEN: usize = 16 << 20;

/// Deepest group nesting tracked for names and visibility. Photoshop itself stops at 10.
const MAX_GROUP_DEPTH: usize = 256;

/// Output rows handed to a thread at a time; also how often the row pass checks for cancellation.
const BAND_ROWS: usize = 32;

/// A rectangle in canvas pixels, `right`/`bottom` exclusive. `i64` so arithmetic on the file's
/// `i32` coordinates can never overflow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub left: i64,
    pub top: i64,
    pub right: i64,
    pub bottom: i64,
}

impl Rect {
    pub fn width(&self) -> i64 {
        (self.right - self.left).max(0)
    }

    pub fn height(&self) -> i64 {
        (self.bottom - self.top).max(0)
    }

    pub fn is_empty(&self) -> bool {
        self.width() == 0 || self.height() == 0
    }

    fn intersect(&self, o: &Rect) -> Rect {
        Rect {
            left: self.left.max(o.left),
            top: self.top.max(o.top),
            right: self.right.min(o.right),
            bottom: self.bottom.min(o.bottom),
        }
    }
}

/// A layer's blend mode, as the four-character key the file stores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlendMode {
    Normal,
    Dissolve,
    Darken,
    Multiply,
    ColorBurn,
    LinearBurn,
    DarkerColor,
    Lighten,
    Screen,
    ColorDodge,
    LinearDodge,
    LighterColor,
    Overlay,
    SoftLight,
    HardLight,
    VividLight,
    LinearLight,
    PinLight,
    HardMix,
    Difference,
    Exclusion,
    Subtract,
    Divide,
    Hue,
    Saturation,
    Color,
    Luminosity,
    PassThrough,
    /// A key this table does not know; shown as the raw key.
    Other([u8; 4]),
}

impl BlendMode {
    pub fn from_key(key: [u8; 4]) -> Self {
        match &key {
            b"norm" => Self::Normal,
            b"diss" => Self::Dissolve,
            b"dark" => Self::Darken,
            b"mul " => Self::Multiply,
            b"idiv" => Self::ColorBurn,
            b"lbrn" => Self::LinearBurn,
            b"dkCl" => Self::DarkerColor,
            b"lite" => Self::Lighten,
            b"scrn" => Self::Screen,
            b"div " => Self::ColorDodge,
            b"lddg" => Self::LinearDodge,
            b"lgCl" => Self::LighterColor,
            b"over" => Self::Overlay,
            b"sLit" => Self::SoftLight,
            b"hLit" => Self::HardLight,
            b"vLit" => Self::VividLight,
            b"lLit" => Self::LinearLight,
            b"pLit" => Self::PinLight,
            b"hMix" => Self::HardMix,
            b"diff" => Self::Difference,
            b"smud" => Self::Exclusion,
            b"fsub" => Self::Subtract,
            b"fdiv" => Self::Divide,
            b"hue " => Self::Hue,
            b"sat " => Self::Saturation,
            b"colr" => Self::Color,
            b"lum " => Self::Luminosity,
            b"pass" => Self::PassThrough,
            _ => Self::Other(key),
        }
    }

    /// The name Photoshop's Layers panel uses.
    pub fn label(&self) -> String {
        let s = match self {
            Self::Normal => "Normal",
            Self::Dissolve => "Dissolve",
            Self::Darken => "Darken",
            Self::Multiply => "Multiply",
            Self::ColorBurn => "Color Burn",
            Self::LinearBurn => "Linear Burn",
            Self::DarkerColor => "Darker Color",
            Self::Lighten => "Lighten",
            Self::Screen => "Screen",
            Self::ColorDodge => "Color Dodge",
            Self::LinearDodge => "Linear Dodge (Add)",
            Self::LighterColor => "Lighter Color",
            Self::Overlay => "Overlay",
            Self::SoftLight => "Soft Light",
            Self::HardLight => "Hard Light",
            Self::VividLight => "Vivid Light",
            Self::LinearLight => "Linear Light",
            Self::PinLight => "Pin Light",
            Self::HardMix => "Hard Mix",
            Self::Difference => "Difference",
            Self::Exclusion => "Exclusion",
            Self::Subtract => "Subtract",
            Self::Divide => "Divide",
            Self::Hue => "Hue",
            Self::Saturation => "Saturation",
            Self::Color => "Color",
            Self::Luminosity => "Luminosity",
            Self::PassThrough => "Pass Through",
            Self::Other(k) => return String::from_utf8_lossy(k).trim().to_string(),
        };
        s.to_string()
    }
}

/// One steppable layer, as the viewer lists it.
#[derive(Debug, Clone)]
pub struct LayerInfo {
    /// The layer's name (the Unicode `luni` name when present).
    pub name: String,
    /// Names of the enclosing groups, outermost first.
    pub group_path: Vec<String>,
    /// The layer's bounds as stored, which may extend past the canvas.
    pub bounds: Rect,
    /// `bounds` clipped to the canvas — the pixels a decode produces. Never empty.
    pub clip: Rect,
    /// The layer's own visibility toggle is off.
    pub hidden: bool,
    /// Some enclosing group's visibility toggle is off.
    pub hidden_by_group: bool,
    /// 0..=255, not applied to the decoded pixels.
    pub opacity: u8,
    /// Not applied to the decoded pixels.
    pub blend: BlendMode,
    /// The layer is clipped to the one below (a clipping mask); not applied.
    pub clipped: bool,
    /// The layer has an enabled layer or vector mask, which *is* applied.
    pub masked: bool,
    record: usize,
}

/// Why a layer did not decode.
#[derive(Debug)]
pub enum LayerError {
    /// The caller's cancel callback asked to stop.
    Cancelled,
    /// The layer's data is malformed or too large.
    Decode(DecodeError),
}

impl std::fmt::Display for LayerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LayerError::Cancelled => write!(f, "cancelled"),
            LayerError::Decode(e) => write!(f, "{e}"),
        }
    }
}

impl From<DecodeError> for LayerError {
    fn from(e: DecodeError) -> Self {
        LayerError::Decode(e)
    }
}

fn malformed(what: impl Into<String>) -> LayerError {
    LayerError::Decode(DecodeError::Malformed(what.into()))
}

/// How a layer decode may use the machine, read at each stage so a job can be sped up (or slowed
/// down) while it runs.
pub struct LayerCtl<'a> {
    /// Threads the next stage may use; 0 means "as many as the core count allows".
    pub threads: &'a (dyn Fn() -> usize + Sync),
    /// Returns true when the result is no longer wanted.
    pub cancel: &'a (dyn Fn() -> bool + Sync),
}

impl std::fmt::Debug for LayerCtl<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LayerCtl")
            .field("threads", &(self.threads)())
            .field("cancelled", &(self.cancel)())
            .finish()
    }
}

fn all_threads() -> usize {
    0
}

fn never() -> bool {
    false
}

impl LayerCtl<'static> {
    /// Every core, never cancelled.
    pub const DEFAULT: LayerCtl<'static> = LayerCtl {
        threads: &all_threads,
        cancel: &never,
    };
}

/// A decoded layer: a buffer the size of its on-canvas bounds, and where those bounds sit.
#[derive(Debug)]
pub struct LayerImage {
    /// Pixels in the document's depth (`Rgba8Unorm` / `Rgba16Unorm` / `Rgba32Float`), already
    /// colour-managed and downscaled exactly as the composite is.
    pub image: DecodedImage,
    /// Top-left of `image` on the canvas, in displayed (post-downscale) pixels.
    pub origin: (u32, u32),
    /// The displayed canvas size (the composite's dimensions).
    pub canvas: (u32, u32),
}

#[derive(Debug, Clone, Copy)]
struct MaskDesc {
    rect: Rect,
    default: u8,
    disabled: bool,
}

#[derive(Debug, Clone, Copy)]
struct ChannelRec {
    id: i16,
    /// Offset of the channel's 2-byte compression word.
    offset: usize,
    /// Length including the compression word.
    len: usize,
}

#[derive(Debug, Clone)]
struct Record {
    rect: Rect,
    channels: Vec<ChannelRec>,
    blend: [u8; 4],
    opacity: u8,
    clipping: u8,
    hidden: bool,
    /// The first mask block: the layer mask, or the vector mask when both exist (channel -2).
    mask_a: Option<MaskDesc>,
    /// The "real" user mask when a layer has both masks (channel -3).
    mask_b: Option<MaskDesc>,
    name: String,
    /// `lsct` section type: 0 normal, 1/2 group, 3 section divider.
    kind: u32,
    /// An adjustment or fill layer: it has no pixels of its own to show.
    no_pixels: bool,
}

/// A PSD's layer stack, ready to decode any layer. Immutable after [`open`](Self::open), and
/// `Send + Sync`: it holds the file's bytes and the parsed records, nothing per-decode.
pub struct PsdLayers {
    bytes: Arc<Vec<u8>>,
    width: u32,
    height: u32,
    depth: u16,
    mode: u16,
    icc: Option<Vec<u8>>,
    records: Vec<Record>,
    layers: Vec<LayerInfo>,
}

impl std::fmt::Debug for PsdLayers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PsdLayers")
            .field("canvas", &(self.width, self.height))
            .field("depth", &self.depth)
            .field("mode", &self.mode)
            .field("records", &self.records.len())
            .field("layers", &self.layers.len())
            .finish()
    }
}

impl PsdLayers {
    /// Parse `bytes`' layer records (metadata only — no pixel data is touched). `Ok(None)` when
    /// the file is a PSD with nothing to step through: no layer section, only groups or
    /// adjustment layers, a colour mode without layers, or a format this module does not read
    /// (PSB). `Err` when the layer section is malformed.
    pub fn open(bytes: Arc<Vec<u8>>) -> Result<Option<Self>, DecodeError> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| Self::open_inner(bytes)))
            .unwrap_or_else(|_| Err(DecodeError::Other("PSD layer parser panicked".into())))
    }

    fn open_inner(bytes: Arc<Vec<u8>>) -> Result<Option<Self>, DecodeError> {
        let b: &[u8] = &bytes;
        let mut c = Cur::new(b, 0, b.len());
        if c.take(4)? != b"8BPS" {
            return Ok(None);
        }
        if c.u16()? != 1 {
            return Ok(None); // PSB (version 2): the composite path cannot open it either
        }
        c.skip(6)?;
        let _channels = c.u16()?;
        let height = c.u32()?;
        let width = c.u32()?;
        let depth = c.u16()?;
        let mode = c.u16()?;
        if !matches!(depth, 8 | 16 | 32)
            || !matches!(
                mode,
                MODE_GRAYSCALE | MODE_RGB | MODE_CMYK | MODE_DUOTONE | MODE_LAB
            )
            || width == 0
            || height == 0
            || width as usize > crate::MAX_DECODE_DIM
            || height as usize > crate::MAX_DECODE_DIM
        {
            return Ok(None);
        }

        let cmd_len = c.u32()? as usize;
        c.skip(cmd_len)?;

        let res_len = c.u32()? as usize;
        let res_start = c.pos;
        c.skip(res_len)?;
        let icc = if matches!(mode, MODE_CMYK | MODE_LAB) {
            // The composite path converts these to RGB and drops their (CMYK/Lab) profile too.
            None
        } else {
            find_icc(b, res_start, res_start + res_len)
        };

        let lm_len = c.u32()? as usize;
        let lm_start = c.pos;
        let lm_end = lm_start
            .checked_add(lm_len)
            .filter(|&e| e <= b.len())
            .ok_or_else(|| DecodeError::Malformed("layer section runs past the file".into()))?;
        if lm_len == 0 {
            return Ok(None);
        }

        let mut lc = Cur::new(b, lm_start, lm_end);
        let li_len = lc.u32()? as usize;
        let li_start = lc.pos;
        let li_end = li_start
            .checked_add(li_len)
            .filter(|&e| e <= lm_end)
            .ok_or_else(|| DecodeError::Malformed("layer info runs past its section".into()))?;
        let mut records = if li_len >= 2 {
            parse_layer_info(b, li_start, li_end)?
        } else {
            Vec::new()
        };

        // Global layer mask info, then the global additional info blocks — where 16/32-bit
        // documents keep their real layer records.
        let mut gc = Cur::new(b, li_end, lm_end);
        if let Ok(glen) = gc.u32() {
            let _ = gc.skip(glen as usize);
            while gc.remaining() >= 12 {
                let sig = gc.take(4)?;
                if sig != b"8BIM" && sig != b"8B64" {
                    break;
                }
                let key: [u8; 4] = gc.take(4)?.try_into().unwrap();
                let len = gc.u32()? as usize;
                let start = gc.pos;
                let Some(end) = start.checked_add(len).filter(|&e| e <= lm_end) else {
                    break;
                };
                if (&key == b"Lr16" || &key == b"Lr32") && len >= 2 {
                    let deep = parse_layer_info(b, start, end)?;
                    if !deep.is_empty() {
                        records = deep;
                    }
                }
                // Blocks here are padded to 4 bytes by Photoshop, to 2 by some writers: take
                // whichever lands on the next signature.
                let pad4 = start + len.div_ceil(4) * 4;
                let pad2 = start + len.div_ceil(2) * 2;
                let at_sig = |p: usize| {
                    b.get(p..p + 4)
                        .is_some_and(|s| s == b"8BIM" || s == b"8B64")
                };
                gc.pos = if at_sig(pad4) { pad4 } else { pad2.min(lm_end) };
            }
        }

        let layers = build_stack(&records, width, height);
        if layers.is_empty() {
            return Ok(None);
        }
        Ok(Some(Self {
            bytes,
            width,
            height,
            depth,
            mode,
            icc,
            records,
            layers,
        }))
    }

    /// The steppable layers, top-most first (the order of Photoshop's Layers panel).
    pub fn layers(&self) -> &[LayerInfo] {
        &self.layers
    }

    /// The document's canvas size, before any downscale.
    pub fn canvas(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Bits per channel: 8, 16 or 32.
    pub fn depth(&self) -> u16 {
        self.depth
    }

    /// Bytes [`decode_layer`](Self::decode_layer) will allocate for layer `index`'s pixels (the
    /// cropped buffer, before any downscale). For scheduling; 0 for an index out of range.
    pub fn layer_bytes(&self, index: usize) -> u64 {
        self.layers.get(index).map_or(0, |l| {
            l.clip.width() as u64 * l.clip.height() as u64 * 4 * (self.depth as u64 / 8)
        })
    }

    /// Decode layer `index` (into [`layers`](Self::layers)). Safe to call from any number of
    /// threads at once, for the same or different layers.
    pub fn decode_layer(
        &self,
        index: usize,
        opts: &DecodeOptions,
        ctl: &LayerCtl,
    ) -> Result<LayerImage, LayerError> {
        let info = self
            .layers
            .get(index)
            .ok_or_else(|| malformed("no such layer"))?;
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.decode_inner(info, opts, ctl)
        }))
        .unwrap_or_else(|_| {
            Err(LayerError::Decode(DecodeError::Other(
                "PSD layer decoder panicked".into(),
            )))
        })
    }

    fn decode_inner(
        &self,
        info: &LayerInfo,
        opts: &DecodeOptions,
        ctl: &LayerCtl,
    ) -> Result<LayerImage, LayerError> {
        let rec = &self.records[info.record];
        let clip = info.clip;
        let (cw, ch) = (clip.width() as usize, clip.height() as usize);
        let bps = self.depth as usize / 8;
        check_dims(cw, ch, 4 * bps, "PSD layer")?;

        let colour_count = match self.mode {
            MODE_GRAYSCALE | MODE_DUOTONE => 1,
            MODE_CMYK => 4,
            _ => 3,
        };

        // Sort the channels into roles. A duplicate is malformed; ids this mode has no use for
        // (spot channels, a K channel in an RGB document) are ignored.
        let mut colour: [Option<ChannelRec>; 4] = [None; 4];
        let mut alpha = None;
        let mut mask_a = None;
        let mut mask_b = None;
        for ch_rec in &rec.channels {
            let slot = match ch_rec.id {
                id @ 0..=3 if (id as usize) < colour_count => &mut colour[id as usize],
                -1 => &mut alpha,
                -2 if rec.mask_a.is_some() => &mut mask_a,
                -3 if rec.mask_b.is_some() => &mut mask_b,
                _ => continue,
            };
            if slot.is_some() {
                return Err(malformed(format!("duplicate channel {}", ch_rec.id)));
            }
            *slot = Some(*ch_rec);
        }

        if (ctl.cancel)() {
            return Err(LayerError::Cancelled);
        }

        // Inflate the ZIP channels (one thread each, they are independent); RAW and RLE are read
        // in place by the row pass.
        let bytes: &[u8] = &self.bytes;
        let depth = self.depth;
        let mut jobs: Vec<(usize, ChannelRec, Rect)> = Vec::new();
        for (i, c) in colour.iter().enumerate() {
            if let Some(c) = c {
                jobs.push((i, *c, rec.rect));
            }
        }
        if let Some(c) = alpha {
            jobs.push((4, c, rec.rect));
        }
        let masks: [Option<(ChannelRec, MaskDesc)>; 2] = [
            mask_a.zip(rec.mask_a).filter(|(_, m)| !m.disabled),
            mask_b.zip(rec.mask_b).filter(|(_, m)| !m.disabled),
        ];
        // A mask with no channel data still applies, as its default colour everywhere.
        let mask_defaults: Vec<MaskDesc> = [rec.mask_a, rec.mask_b]
            .iter()
            .zip([mask_a, mask_b])
            .filter_map(|(m, c)| m.filter(|m| !m.disabled && c.is_none()))
            .collect();
        for (k, m) in masks.iter().enumerate() {
            if let Some((c, d)) = m {
                jobs.push((5 + k, *c, d.rect));
            }
        }

        let mut sources: Vec<(usize, Plane)> = Vec::with_capacity(jobs.len());
        // At most `threads` workers, each taking every `workers`-th channel.
        let workers = resolve_threads((ctl.threads)()).min(jobs.len());
        let prepared: Vec<Result<(usize, Plane), LayerError>> = if workers > 1 {
            std::thread::scope(|s| {
                let handles: Vec<_> = (0..workers)
                    .map(|first| {
                        let jobs = &jobs;
                        s.spawn(move || {
                            jobs.iter()
                                .skip(first)
                                .step_by(workers)
                                .map(|&(slot, c, r)| {
                                    prepare_plane(bytes, c, r, depth, ctl.cancel).map(|p| (slot, p))
                                })
                                .collect::<Vec<_>>()
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .flat_map(|h| {
                        h.join()
                            .unwrap_or_else(|_| vec![Err(malformed("channel decode panicked"))])
                    })
                    .collect()
            })
        } else {
            jobs.iter()
                .map(|&(slot, c, r)| {
                    prepare_plane(bytes, c, r, depth, ctl.cancel).map(|p| (slot, p))
                })
                .collect()
        };
        for p in prepared {
            sources.push(p?);
        }
        let plane = |slot: usize| sources.iter().find(|(s, _)| *s == slot).map(|(_, p)| p);

        // The row pass: every band of output rows writes its colour lanes, converts the colour
        // mode, writes alpha and multiplies in the masks.
        let row_bytes = cw * 4 * bps;
        let mut out = vec![0u8; row_bytes * ch];
        let pass = RowPass {
            mode: self.mode,
            depth,
            clip,
            colour: [plane(0), plane(1), plane(2), plane(3)],
            colour_count,
            alpha: plane(4),
            masks: [
                masks[0].map(|(_, d)| d).zip(plane(5)),
                masks[1].map(|(_, d)| d).zip(plane(6)),
            ],
            mask_defaults: &mask_defaults,
        };
        let failed: Mutex<Option<String>> = Mutex::new(None);
        let stop = AtomicBool::new(false);
        let next_band = AtomicUsize::new(0);
        // Bands of rows, each locked by exactly one thread (handed out once by `next_band`), so
        // the locks never contend; they only make the disjoint `&mut` split expressible.
        let bands: Vec<Mutex<&mut [u8]>> = out
            .chunks_mut(BAND_ROWS * row_bytes)
            .map(Mutex::new)
            .collect();
        let run = || {
            let mut scratch = Vec::new();
            loop {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                let band = next_band.fetch_add(1, Ordering::Relaxed);
                if band >= bands.len() {
                    return;
                }
                if (ctl.cancel)() {
                    stop.store(true, Ordering::Relaxed);
                    return;
                }
                let mut rows = bands[band].lock().unwrap();
                for (k, row) in rows.chunks_mut(row_bytes).enumerate() {
                    if let Err(e) = pass.row(band * BAND_ROWS + k, row, &mut scratch) {
                        *failed.lock().unwrap() = Some(e);
                        stop.store(true, Ordering::Relaxed);
                        return;
                    }
                }
            }
        };
        let row_threads = resolve_threads((ctl.threads)()).min(bands.len()).max(1);
        if row_threads > 1 {
            std::thread::scope(|s| {
                for _ in 1..row_threads {
                    s.spawn(run);
                }
                run();
            });
        } else {
            run();
        }
        drop(bands);
        if let Some(e) = failed.into_inner().unwrap() {
            return Err(malformed(e));
        }
        if stop.load(Ordering::Relaxed) || (ctl.cancel)() {
            return Err(LayerError::Cancelled);
        }

        let grey = matches!(self.mode, MODE_GRAYSCALE | MODE_DUOTONE);
        let mut img = DecodedImage {
            pixels: out,
            width: cw as u32,
            height: ch as u32,
            format: match depth {
                16 => PixelFormat::Rgba16Unorm,
                32 => PixelFormat::Rgba32Float,
                _ => PixelFormat::Rgba8Unorm,
            },
            bit_depth: depth as u8,
            channels: if grey { 2 } else { 4 },
            alpha_opaque: false,
            icc: self.icc.clone(),
            source_format: "PSD",
            downscaled_from: None,
            source_mips: None,
            layout: None,
            animation: None,
        };

        let stage_threads = resolve_threads((ctl.threads)());
        if opts.honor_icc {
            crate::with_max_threads(stage_threads, || crate::icc::apply(&mut img));
        }
        if (ctl.cancel)() {
            return Err(LayerError::Cancelled);
        }
        let origin = (clip.left as u32, clip.top as u32);
        let (canvas, origin) = crate::downscale::rect_to_fit(
            &mut img,
            (self.width, self.height),
            origin,
            opts.max_dim,
        );
        let full_canvas = origin == (0, 0) && (img.width, img.height) == canvas;
        img.alpha_opaque =
            full_canvas && crate::with_max_threads(stage_threads, || crate::alpha_is_opaque(&img));
        Ok(LayerImage {
            image: img,
            origin,
            canvas,
        })
    }
}

fn resolve_threads(requested: usize) -> usize {
    let avail = std::thread::available_parallelism().map_or(1, |n| n.get().min(8));
    if requested == 0 {
        avail
    } else {
        requested.min(avail).max(1)
    }
}

/// One channel's pixels, ready for the row pass.
enum Plane<'a> {
    /// No pixel data (zero-size bounds, or a 2-byte channel): the channel's default everywhere.
    Empty,
    /// Uncompressed rows, as stored (big-endian samples).
    Raw {
        data: &'a [u8],
        w: usize,
        rect: Rect,
    },
    /// PackBits rows; `rows[y]` is the byte range of row `y` in `data`.
    Rle {
        data: &'a [u8],
        rows: Vec<(usize, usize)>,
        w: usize,
        rect: Rect,
    },
    /// Inflated (and de-predicted) rows, big-endian samples like `Raw`.
    Owned { data: Vec<u8>, w: usize, rect: Rect },
}

impl Plane<'_> {
    fn rect(&self) -> Option<Rect> {
        match self {
            Plane::Empty => None,
            Plane::Raw { rect, .. } | Plane::Rle { rect, .. } | Plane::Owned { rect, .. } => {
                Some(*rect)
            }
        }
    }

    /// Row `y` (relative to the plane's rect) as big-endian samples. RLE rows are unpacked into
    /// `scratch`.
    fn row<'s>(
        &'s self,
        y: usize,
        bps: usize,
        scratch: &'s mut Vec<u8>,
    ) -> Result<&'s [u8], String> {
        match self {
            Plane::Empty => Err("empty plane has no rows".into()),
            Plane::Raw { data, w, .. } => {
                let n = w * bps;
                Ok(&data[y * n..(y + 1) * n])
            }
            Plane::Owned { data, w, .. } => {
                let n = w * bps;
                Ok(&data[y * n..(y + 1) * n])
            }
            Plane::Rle { data, rows, w, .. } => {
                let n = w * bps;
                let (start, len) = rows[y];
                scratch.clear();
                scratch.resize(n, 0);
                unpack_bits(&data[start..start + len], scratch)?;
                Ok(&scratch[..])
            }
        }
    }
}

/// PackBits-decode one row into `out` (its exact length). Malformed input — a run or literal
/// past either end — is an error rather than a short or overlong row.
fn unpack_bits(src: &[u8], out: &mut [u8]) -> Result<(), String> {
    let mut i = 0;
    let mut o = 0;
    while o < out.len() {
        let Some(&n) = src.get(i) else {
            return Err("RLE row ends early".into());
        };
        i += 1;
        if n == 0x80 {
            continue;
        }
        if n < 0x80 {
            let count = n as usize + 1;
            let (Some(s), Some(d)) = (src.get(i..i + count), out.get_mut(o..o + count)) else {
                return Err("RLE literal overruns its row".into());
            };
            d.copy_from_slice(s);
            i += count;
            o += count;
        } else {
            let count = 257 - n as usize;
            let (Some(&v), Some(d)) = (src.get(i), out.get_mut(o..o + count)) else {
                return Err("RLE run overruns its row".into());
            };
            d.fill(v);
            i += 1;
            o += count;
        }
    }
    Ok(())
}

/// Validate one channel and make its rows addressable: RAW and RLE in place, ZIP inflated.
fn prepare_plane<'a>(
    bytes: &'a [u8],
    c: ChannelRec,
    rect: Rect,
    depth: u16,
    cancel: &(dyn Fn() -> bool + Sync),
) -> Result<Plane<'a>, LayerError> {
    let bps = depth as usize / 8;
    let (w, h) = (rect.width() as usize, rect.height() as usize);
    if c.len <= 2 || w == 0 || h == 0 {
        return Ok(Plane::Empty);
    }
    check_dims(w, h, bps, "PSD layer channel")?;
    let end = c
        .offset
        .checked_add(c.len)
        .filter(|&e| e <= bytes.len())
        .ok_or_else(|| malformed("channel data runs past the file"))?;
    let comp = u16::from_be_bytes([bytes[c.offset], bytes[c.offset + 1]]);
    let body = &bytes[c.offset + 2..end];
    let plane_bytes = w * h * bps;
    match comp {
        0 => {
            if body.len() < plane_bytes {
                return Err(malformed("raw channel is short"));
            }
            Ok(Plane::Raw {
                data: &body[..plane_bytes],
                w,
                rect,
            })
        }
        1 => {
            let table = h * 2;
            if body.len() < table {
                return Err(malformed("RLE row table is short"));
            }
            let mut rows = Vec::with_capacity(h);
            let mut pos = table;
            for y in 0..h {
                let n = u16::from_be_bytes([body[y * 2], body[y * 2 + 1]]) as usize;
                if pos + n > body.len() {
                    return Err(malformed("RLE rows run past the channel"));
                }
                rows.push((pos, n));
                pos += n;
            }
            Ok(Plane::Rle {
                data: body,
                rows,
                w,
                rect,
            })
        }
        2 | 3 => {
            let mut data = inflate(body, plane_bytes, cancel)?;
            // psd_sdk treats 32-bit ZIP as ZIP-with-prediction too; so does Photoshop's output.
            if comp == 3 || depth == 32 {
                unpredict(&mut data, w, h, depth);
            }
            Ok(Plane::Owned { data, w, rect })
        }
        other => Err(malformed(format!("unknown channel compression {other}"))),
    }
}

/// Inflate a zlib stream into exactly `size` bytes, checking `cancel` between 1 MiB input slices.
/// A stream that ends early leaves zeros (not uninitialised memory) and is an error.
fn inflate(
    src: &[u8],
    size: usize,
    cancel: &(dyn Fn() -> bool + Sync),
) -> Result<Vec<u8>, LayerError> {
    use miniz_oxide::inflate::core::inflate_flags::{
        TINFL_FLAG_HAS_MORE_INPUT, TINFL_FLAG_PARSE_ZLIB_HEADER,
        TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF,
    };
    use miniz_oxide::inflate::core::{decompress, DecompressorOxide};
    use miniz_oxide::inflate::TINFLStatus;

    const SLICE: usize = 1 << 20;
    let mut out = vec![0u8; size];
    let mut state = Box::<DecompressorOxide>::default();
    let mut in_pos = 0;
    let mut out_pos = 0;
    loop {
        if cancel() {
            return Err(LayerError::Cancelled);
        }
        let end = (in_pos + SLICE).min(src.len());
        let more = end < src.len();
        let mut flags = TINFL_FLAG_PARSE_ZLIB_HEADER | TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF;
        if more {
            flags |= TINFL_FLAG_HAS_MORE_INPUT;
        }
        let (status, read, written) =
            decompress(&mut state, &src[in_pos..end], &mut out, out_pos, flags);
        in_pos += read;
        out_pos += written;
        match status {
            TINFLStatus::Done => break,
            // The output is exactly the plane: anything the stream has beyond it is ignored.
            TINFLStatus::HasMoreOutput if out_pos == size => break,
            TINFLStatus::NeedsMoreInput if more => continue,
            _ => return Err(malformed("channel ZIP stream is corrupt or short")),
        }
    }
    if out_pos != size {
        return Err(malformed("channel ZIP stream is short"));
    }
    Ok(out)
}

/// Undo ZIP-with-prediction in place, leaving big-endian samples like an uncompressed channel.
/// Mirrors psd_sdk's `ApplyPrediction`: a per-row delta on bytes (8-bit) or 16-bit words, and for
/// 32-bit a byte delta over the row's four byte planes, which are then re-interleaved.
fn unpredict(data: &mut [u8], w: usize, h: usize, depth: u16) {
    match depth {
        8 => {
            for row in data.chunks_mut(w).take(h) {
                for x in 1..row.len() {
                    row[x] = row[x].wrapping_add(row[x - 1]);
                }
            }
        }
        16 => {
            for row in data.chunks_mut(w * 2).take(h) {
                let mut prev = 0u16;
                for (x, px) in row.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                    let d = u16::from_be_bytes([px[0], px[1]]);
                    let v = if x == 0 { d } else { d.wrapping_add(prev) };
                    px.copy_from_slice(&v.to_be_bytes());
                    prev = v;
                }
            }
        }
        _ => {
            let mut tmp = vec![0u8; w * 4];
            for row in data.chunks_mut(w * 4).take(h) {
                for x in 1..row.len() {
                    row[x] = row[x].wrapping_add(row[x - 1]);
                }
                tmp.copy_from_slice(row);
                for x in 0..w {
                    row[x * 4] = tmp[x];
                    row[x * 4 + 1] = tmp[w + x];
                    row[x * 4 + 2] = tmp[2 * w + x];
                    row[x * 4 + 3] = tmp[3 * w + x];
                }
            }
        }
    }
}

/// Everything one output row needs.
struct RowPass<'a> {
    mode: u16,
    depth: u16,
    clip: Rect,
    colour: [Option<&'a Plane<'a>>; 4],
    colour_count: usize,
    alpha: Option<&'a Plane<'a>>,
    masks: [Option<(MaskDesc, &'a Plane<'a>)>; 2],
    mask_defaults: &'a [MaskDesc],
}

impl RowPass<'_> {
    fn row(&self, y: usize, out: &mut [u8], scratch: &mut Vec<u8>) -> Result<(), String> {
        let bps = self.depth as usize / 8;
        let cw = self.clip.width() as usize;
        let cy = self.clip.top + y as i64;

        // 1. Colour channels into their lanes (CMYK's K into lane 3 for the conversion below).
        for lane in 0..self.colour_count {
            match self.colour[lane] {
                Some(p) => self.put_lane(p, lane, cy, out, scratch)?,
                None => fill_lane(out, lane, bps, Sample::Zero),
            }
        }

        // 2. Colour mode to RGB, the same arithmetic as wrapper.cpp's composite path.
        convert_row(self.mode, self.depth, out, cw);

        // 3. Alpha: the transparency channel, or opaque.
        match self.alpha {
            Some(p) => self.put_lane(p, 3, cy, out, scratch)?,
            None => fill_lane(out, 3, bps, Sample::Max),
        }

        // 4. Masks multiply alpha.
        for (desc, plane) in self.masks.iter().flatten() {
            self.apply_mask(desc, plane, cy, out, scratch)?;
        }
        for desc in self.mask_defaults {
            let m = desc.default as f32 / 255.0;
            multiply_alpha_span(out, 0, cw, self.depth, |_| m);
        }
        Ok(())
    }

    /// Copy the clipped columns of plane `p`'s canvas row `cy` into lane `lane`.
    fn put_lane(
        &self,
        p: &Plane,
        lane: usize,
        cy: i64,
        out: &mut [u8],
        scratch: &mut Vec<u8>,
    ) -> Result<(), String> {
        let bps = self.depth as usize / 8;
        let Some(rect) = p.rect() else {
            // A colour or transparency channel with no data: psd_sdk's default is 0 (and 0 alpha).
            fill_lane(out, lane, bps, Sample::Zero);
            return Ok(());
        };
        let src = p.row((cy - rect.top) as usize, bps, scratch)?;
        let x0 = (self.clip.left - rect.left) as usize;
        let cw = self.clip.width() as usize;
        let src = &src[x0 * bps..(x0 + cw) * bps];
        let stride = 4 * bps;
        match bps {
            1 => {
                for (d, s) in out.chunks_exact_mut(stride).zip(src) {
                    d[lane] = *s;
                }
            }
            2 => {
                for (d, s) in out.chunks_exact_mut(stride).zip(src.as_chunks::<2>().0) {
                    let v = u16::from_be_bytes([s[0], s[1]]).to_ne_bytes();
                    d[lane * 2..lane * 2 + 2].copy_from_slice(&v);
                }
            }
            _ => {
                for (d, s) in out.chunks_exact_mut(stride).zip(src.as_chunks::<4>().0) {
                    let v = f32::from_be_bytes([s[0], s[1], s[2], s[3]]).to_ne_bytes();
                    d[lane * 4..lane * 4 + 4].copy_from_slice(&v);
                }
            }
        }
        Ok(())
    }

    fn apply_mask(
        &self,
        desc: &MaskDesc,
        plane: &Plane,
        cy: i64,
        out: &mut [u8],
        scratch: &mut Vec<u8>,
    ) -> Result<(), String> {
        let bps = self.depth as usize / 8;
        let cw = self.clip.width() as usize;
        let default = desc.default as f32 / 255.0;
        let rect = desc.rect;
        // A mask whose channel holds no pixels is its default colour everywhere.
        if plane.rect().is_none() || cy < rect.top || cy >= rect.bottom {
            multiply_alpha_span(out, 0, cw, self.depth, |_| default);
            return Ok(());
        }
        let src = plane.row((cy - rect.top) as usize, bps, scratch)?;
        let depth = self.depth;
        let left = self.clip.left;
        multiply_alpha_span(out, 0, cw, depth, |x| {
            let cx = left + x as i64;
            if cx < rect.left || cx >= rect.right {
                return default;
            }
            let i = (cx - rect.left) as usize * bps;
            match bps {
                1 => src[i] as f32 / 255.0,
                2 => u16::from_be_bytes([src[i], src[i + 1]]) as f32 / 65535.0,
                _ => {
                    f32::from_be_bytes([src[i], src[i + 1], src[i + 2], src[i + 3]]).clamp(0.0, 1.0)
                }
            }
        });
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum Sample {
    Zero,
    Max,
}

fn fill_lane(out: &mut [u8], lane: usize, bps: usize, v: Sample) {
    let stride = 4 * bps;
    match (bps, v) {
        (1, Sample::Zero) => out.chunks_exact_mut(stride).for_each(|d| d[lane] = 0),
        (1, Sample::Max) => out.chunks_exact_mut(stride).for_each(|d| d[lane] = 255),
        (2, v) => {
            let b = match v {
                Sample::Zero => 0u16,
                Sample::Max => 65535,
            }
            .to_ne_bytes();
            out.chunks_exact_mut(stride)
                .for_each(|d| d[lane * 2..lane * 2 + 2].copy_from_slice(&b));
        }
        (_, v) => {
            let b = match v {
                Sample::Zero => 0f32,
                Sample::Max => 1.0,
            }
            .to_ne_bytes();
            out.chunks_exact_mut(stride)
                .for_each(|d| d[lane * 4..lane * 4 + 4].copy_from_slice(&b));
        }
    }
}

/// `alpha *= m(x)` over pixels `x0..x1` of a row, `m` in 0..=1.
fn multiply_alpha_span(out: &mut [u8], x0: usize, x1: usize, depth: u16, m: impl Fn(usize) -> f32) {
    let bps = depth as usize / 8;
    let stride = 4 * bps;
    for x in x0..x1 {
        let px = &mut out[x * stride..(x + 1) * stride];
        let f = m(x);
        if f >= 1.0 {
            continue;
        }
        match bps {
            1 => px[3] = (px[3] as f32 * f + 0.5) as u8,
            2 => {
                let a = u16::from_ne_bytes([px[6], px[7]]);
                px[6..8].copy_from_slice(&((a as f32 * f + 0.5) as u16).to_ne_bytes());
            }
            _ => {
                let a = f32::from_ne_bytes([px[12], px[13], px[14], px[15]]);
                px[12..16].copy_from_slice(&(a * f).to_ne_bytes());
            }
        }
    }
}

// --- colour conversion (mirrors wrapper.cpp) ---------------------------------------------------

fn linear_to_srgb(c: f32) -> f32 {
    if c <= 0.0 {
        return 0.0;
    }
    if c >= 1.0 {
        return 1.0;
    }
    if c <= 0.0031308 {
        12.92 * c
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

// The constants are wrapper.cpp's, digit for digit, so layers and composite agree exactly.
#[allow(clippy::excessive_precision)]
fn lab_to_srgb(l: f32, a: f32, b: f32) -> [f32; 3] {
    let l = l * 100.0;
    let a = a * 255.0 - 128.0;
    let b = b * 255.0 - 128.0;
    let fy = (l + 16.0) / 116.0;
    let fx = fy + a / 500.0;
    let fz = fy - b / 200.0;
    let d = 6.0f32 / 29.0;
    let finv = |f: f32| {
        if f > d {
            f * f * f
        } else {
            3.0 * d * d * (f - 4.0 / 29.0)
        }
    };
    let x = 0.96422 * finv(fx);
    let y = finv(fy);
    let z = 0.82521 * finv(fz);
    [
        linear_to_srgb(3.1338561 * x - 1.6168667 * y - 0.4906146 * z),
        linear_to_srgb(-0.9787684 * x + 1.9161415 * y + 0.0334540 * z),
        linear_to_srgb(0.0719453 * x - 0.2289914 * y + 1.4052427 * z),
    ]
}

fn read_norm(px: &[u8], lane: usize, depth: u16) -> f32 {
    match depth {
        8 => px[lane] as f32 * (1.0 / 255.0),
        16 => u16::from_ne_bytes([px[lane * 2], px[lane * 2 + 1]]) as f32 * (1.0 / 65535.0),
        _ => f32::from_ne_bytes(px[lane * 4..lane * 4 + 4].try_into().unwrap()),
    }
}

fn store_norm(px: &mut [u8], lane: usize, depth: u16, v: f32) {
    match depth {
        8 => px[lane] = (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8,
        16 => px[lane * 2..lane * 2 + 2]
            .copy_from_slice(&((v.clamp(0.0, 1.0) * 65535.0 + 0.5) as u16).to_ne_bytes()),
        _ => px[lane * 4..lane * 4 + 4].copy_from_slice(&v.to_ne_bytes()),
    }
}

/// Convert one row's colour lanes to RGB in place. RGB needs nothing; greyscale replicates;
/// CMYK (stored inverted, K parked in lane 3) and Lab go through float like the composite.
fn convert_row(mode: u16, depth: u16, out: &mut [u8], cw: usize) {
    let bps = depth as usize / 8;
    let stride = 4 * bps;
    match mode {
        MODE_GRAYSCALE | MODE_DUOTONE => {
            for px in out.chunks_exact_mut(stride).take(cw) {
                let (g, rest) = px.split_at_mut(bps);
                rest[..bps].copy_from_slice(g);
                rest[bps..2 * bps].copy_from_slice(g);
            }
        }
        MODE_CMYK => {
            for px in out.chunks_exact_mut(stride).take(cw) {
                let k = read_norm(px, 3, depth);
                for lane in 0..3 {
                    let v = read_norm(px, lane, depth) * k;
                    store_norm(px, lane, depth, v);
                }
            }
        }
        MODE_LAB => {
            for px in out.chunks_exact_mut(stride).take(cw) {
                let rgb = lab_to_srgb(
                    read_norm(px, 0, depth),
                    read_norm(px, 1, depth),
                    read_norm(px, 2, depth),
                );
                for (lane, v) in rgb.into_iter().enumerate() {
                    store_norm(px, lane, depth, v);
                }
            }
        }
        _ => {}
    }
}

// --- parsing ------------------------------------------------------------------------------------

/// A bounds-checked big-endian reader over `b[pos..end]`.
struct Cur<'a> {
    b: &'a [u8],
    pos: usize,
    end: usize,
}

fn short() -> DecodeError {
    DecodeError::Malformed("PSD layer section is truncated".into())
}

impl<'a> Cur<'a> {
    fn new(b: &'a [u8], pos: usize, end: usize) -> Self {
        Self {
            b,
            pos,
            end: end.min(b.len()),
        }
    }

    fn remaining(&self) -> usize {
        self.end.saturating_sub(self.pos)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if n > self.remaining() {
            return Err(short());
        }
        let s = &self.b[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn skip(&mut self, n: usize) -> Result<(), DecodeError> {
        self.take(n).map(|_| ())
    }

    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn i16(&mut self) -> Result<i16, DecodeError> {
        Ok(i16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn i32(&mut self) -> Result<i32, DecodeError> {
        Ok(i32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn rect(&mut self) -> Result<Rect, DecodeError> {
        let top = self.i32()? as i64;
        let left = self.i32()? as i64;
        let bottom = self.i32()? as i64;
        let right = self.i32()? as i64;
        Ok(Rect {
            left,
            top,
            right,
            bottom,
        })
    }
}

/// The first ICC profile (resource 1039) in the image resources section.
fn find_icc(b: &[u8], start: usize, end: usize) -> Option<Vec<u8>> {
    let mut c = Cur::new(b, start, end);
    while c.remaining() >= 12 {
        let sig = c.take(4).ok()?;
        if sig != b"8BIM" && sig != b"psdM" {
            return None;
        }
        let id = c.u16().ok()?;
        let name_len = c.u8().ok()? as usize;
        c.skip((name_len + 1).div_ceil(2) * 2 - 1).ok()?;
        let size = c.u32().ok()? as usize;
        let data = c.take(size).ok()?;
        if size % 2 == 1 {
            let _ = c.skip(1);
        }
        if id == 1039 {
            return (size > 0 && size <= MAX_ICC_LEN).then(|| data.to_vec());
        }
    }
    None
}

/// Additional-info keys of layers that have no pixels to show: adjustment and fill layers.
const NO_PIXEL_KEYS: &[&[u8; 4]] = &[
    b"SoCo", b"GdFl", b"PtFl", b"brit", b"levl", b"curv", b"expA", b"vibA", b"hue ", b"hue2",
    b"blnc", b"blwh", b"phfl", b"mixr", b"clrL", b"nvrt", b"post", b"thrs", b"grdm", b"selc",
];

/// Parse a layer info block (layer count, records, then the channel data) from `b[start..end]`.
fn parse_layer_info(b: &[u8], start: usize, end: usize) -> Result<Vec<Record>, DecodeError> {
    let mut c = Cur::new(b, start, end);
    let count = c.i16()?.unsigned_abs() as usize;
    let mut records = Vec::with_capacity(count);
    for _ in 0..count {
        records.push(parse_record(&mut c)?);
    }
    // Channel data follows the records, in record order. Lengths are checked against the block
    // here and against the file again before any decode.
    let mut pos = c.pos;
    for r in &mut records {
        for ch in &mut r.channels {
            ch.offset = pos;
            pos = pos
                .checked_add(ch.len)
                .filter(|&p| p <= end)
                .ok_or_else(|| {
                    DecodeError::Malformed("layer channel data runs past its block".into())
                })?;
        }
    }
    Ok(records)
}

fn parse_record(c: &mut Cur) -> Result<Record, DecodeError> {
    let rect = c.rect()?;
    let nch = c.u16()? as usize;
    let mut channels = Vec::with_capacity(nch.min(64));
    for _ in 0..nch {
        let id = c.i16()?;
        let len = c.u32()? as usize;
        channels.push(ChannelRec { id, offset: 0, len });
    }
    if c.take(4)? != b"8BIM" {
        return Err(DecodeError::Malformed(
            "layer record signature is not 8BIM".into(),
        ));
    }
    let blend: [u8; 4] = c.take(4)?.try_into().unwrap();
    let opacity = c.u8()?;
    let clipping = c.u8()?;
    let flags = c.u8()?;
    let _filler = c.u8()?;
    let extra_len = c.u32()? as usize;
    let extra_end = c
        .pos
        .checked_add(extra_len)
        .filter(|&e| e <= c.end)
        .ok_or_else(short)?;
    let mut x = Cur::new(c.b, c.pos, extra_end);

    // Layer mask / adjustment layer data. Layout per psd_sdk (which matches what Photoshop
    // writes): rect, default colour, flags, [parameters if flagged and the block is short],
    // then — when 18+ bytes remain — the "real" mask's flags, default colour and rect.
    let mut mask_a = None;
    let mut mask_b = None;
    let mlen = x.u32()? as usize;
    let mstart = x.pos;
    let mend = mstart
        .checked_add(mlen)
        .filter(|&e| e <= extra_end)
        .ok_or_else(short)?;
    if mlen >= 18 {
        let mut m = Cur::new(c.b, mstart, mend);
        let rect = m.rect()?;
        let default = m.u8()?;
        let mflags = m.u8()?;
        mask_a = Some(MaskDesc {
            rect,
            default,
            disabled: mflags & 2 != 0,
        });
        if mflags & 0x10 != 0 && mlen <= 28 {
            let pflags = m.u8()?;
            let skip = (pflags & 1 != 0) as usize
                + (pflags & 2 != 0) as usize * 8
                + (pflags & 4 != 0) as usize
                + (pflags & 8 != 0) as usize * 8;
            m.skip(skip.min(m.remaining()))?;
        }
        if m.remaining() >= 18 {
            let rflags = m.u8()?;
            let rdefault = m.u8()?;
            let rrect = m.rect()?;
            mask_b = Some(MaskDesc {
                rect: rrect,
                default: rdefault,
                disabled: rflags & 2 != 0,
            });
        }
    }
    x.pos = mend;

    let ranges = x.u32()? as usize;
    x.skip(ranges)?;

    let name_len = x.u8()? as usize;
    let name_bytes = x.take(name_len.min(x.remaining()))?;
    let mut name: String = name_bytes.iter().map(|&ch| ch as char).collect();
    let padded = (name_len + 1).div_ceil(4) * 4;
    x.pos = (x.pos + padded - 1 - name_bytes.len()).min(extra_end);

    let mut kind = 0;
    let mut no_pixels = false;
    while x.remaining() >= 12 {
        let sig = x.take(4)?;
        if sig != b"8BIM" && sig != b"8B64" {
            break;
        }
        let key: [u8; 4] = x.take(4)?.try_into().unwrap();
        let len = x.u32()? as usize;
        let start = x.pos;
        let Some(end) = start.checked_add(len).filter(|&e| e <= extra_end) else {
            break;
        };
        let mut d = Cur::new(c.b, start, end);
        match &key {
            b"luni" => {
                if let Ok(n) = d.u32() {
                    let n = (n as usize).min(d.remaining() / 2);
                    let units: Vec<u16> = (0..n).filter_map(|_| d.u16().ok()).collect();
                    let s = String::from_utf16_lossy(&units);
                    let s = s.trim_end_matches('\0');
                    if !s.is_empty() {
                        name = s.to_string();
                    }
                }
            }
            b"lsct" | b"lsdk" => {
                if let Ok(t) = d.u32() {
                    kind = t;
                }
            }
            k if NO_PIXEL_KEYS.contains(&k) => no_pixels = true,
            _ => {}
        }
        x.pos = (start + len.div_ceil(2) * 2).min(extra_end);
    }
    c.pos = extra_end;

    Ok(Record {
        rect,
        channels,
        blend,
        opacity,
        clipping,
        hidden: flags & 2 != 0,
        mask_a,
        mask_b,
        name,
        kind,
        no_pixels,
    })
}

/// Turn the records (bottom-most first) into the steppable list (top-most first): resolve group
/// membership from the folder/divider records, drop groups, dividers, adjustment/fill layers and
/// layers with nothing on the canvas.
fn build_stack(records: &[Record], width: u32, height: u32) -> Vec<LayerInfo> {
    let canvas = Rect {
        left: 0,
        top: 0,
        right: width as i64,
        bottom: height as i64,
    };
    let mut stack: Vec<usize> = Vec::new();
    // Depth beyond what is tracked: the dividers that close those groups must not pop real ones.
    let mut untracked = 0usize;
    let mut out = Vec::new();
    for i in (0..records.len()).rev() {
        let r = &records[i];
        match r.kind {
            1 | 2 => {
                if stack.len() < MAX_GROUP_DEPTH {
                    stack.push(i);
                } else {
                    untracked += 1;
                }
                continue;
            }
            3 => {
                if untracked > 0 {
                    untracked -= 1;
                } else {
                    stack.pop();
                }
                continue;
            }
            _ => {}
        }
        if r.no_pixels {
            continue;
        }
        let clip = r.rect.intersect(&canvas);
        if r.rect.is_empty() || clip.is_empty() {
            continue;
        }
        let masked = [r.mask_a, r.mask_b].iter().flatten().any(|m| !m.disabled);
        out.push(LayerInfo {
            name: r.name.clone(),
            group_path: stack.iter().map(|&g| records[g].name.clone()).collect(),
            bounds: r.rect,
            clip,
            hidden: r.hidden,
            hidden_by_group: stack.iter().any(|&g| records[g].hidden),
            opacity: r.opacity,
            blend: BlendMode::from_key(r.blend),
            clipped: r.clipping != 0,
            masked,
            record: i,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(name: &str, kind: u32, hidden: bool, rect: Rect) -> Record {
        Record {
            rect,
            channels: Vec::new(),
            blend: *b"norm",
            opacity: 255,
            clipping: 0,
            hidden,
            mask_a: None,
            mask_b: None,
            name: name.into(),
            kind,
            no_pixels: false,
        }
    }

    const R: Rect = Rect {
        left: 0,
        top: 0,
        right: 10,
        bottom: 10,
    };

    /// Records are bottom-first; groups open above their children and close below them.
    #[test]
    fn stack_is_top_first_with_group_paths() {
        let records = vec![
            rec("bg", 0, false, R),
            rec("</g>", 3, false, Rect::default()),
            rec("child-bottom", 0, false, R),
            rec("child-top", 0, true, R),
            rec("Group", 1, true, Rect::default()),
            rec("top", 0, false, R),
        ];
        let s = build_stack(&records, 10, 10);
        let names: Vec<_> = s.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["top", "child-top", "child-bottom", "bg"]);
        assert_eq!(s[1].group_path, ["Group"]);
        assert!(s[1].hidden && s[1].hidden_by_group);
        assert!(!s[2].hidden && s[2].hidden_by_group);
        assert!(s[0].group_path.is_empty() && !s[0].hidden_by_group);
        assert!(s[3].group_path.is_empty());
    }

    /// Off-canvas, empty and adjustment layers are not steppable; a partly-off layer is clipped.
    #[test]
    fn stack_skips_layers_with_nothing_to_show() {
        let mut adj = rec("Levels", 0, false, R);
        adj.no_pixels = true;
        let off = Rect {
            left: 20,
            top: 0,
            right: 30,
            bottom: 10,
        };
        let partial = Rect {
            left: -5,
            top: -5,
            right: 5,
            bottom: 50,
        };
        let records = vec![
            rec("empty", 0, false, Rect::default()),
            rec("off", 0, false, off),
            adj,
            rec("partial", 0, false, partial),
        ];
        let s = build_stack(&records, 10, 10);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].name, "partial");
        assert_eq!(
            s[0].clip,
            Rect {
                left: 0,
                top: 0,
                right: 5,
                bottom: 10
            }
        );
    }

    /// More dividers than groups, and nesting past the tracked depth, never panic and never
    /// attribute a layer to the wrong group.
    #[test]
    fn unbalanced_and_deep_hierarchies_are_tolerated() {
        let mut records = vec![rec("bottom", 0, false, R)];
        for _ in 0..5 {
            records.push(rec("</g>", 3, false, Rect::default()));
        }
        records.push(rec("mid", 0, false, R));
        for i in 0..(MAX_GROUP_DEPTH + 10) {
            records.insert(0, rec("</g>", 3, false, Rect::default()));
            records.push(rec(&format!("g{i}"), 1, false, Rect::default()));
        }
        records.push(rec("top", 0, false, R));
        let s = build_stack(&records, 10, 10);
        let names: Vec<_> = s.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["top", "mid", "bottom"]);
        assert!(s[0].group_path.is_empty());
        assert_eq!(s[1].group_path.len(), MAX_GROUP_DEPTH);
    }

    #[test]
    fn blend_keys_round_trip_to_labels() {
        assert_eq!(BlendMode::from_key(*b"mul ").label(), "Multiply");
        assert_eq!(BlendMode::from_key(*b"pass"), BlendMode::PassThrough);
        assert_eq!(BlendMode::from_key(*b"zzzz").label(), "zzzz");
    }

    #[test]
    fn unpack_bits_refuses_overruns() {
        let mut out = [0u8; 4];
        unpack_bits(&[0xFD, 7], &mut out).unwrap(); // a 4-run (257 - 0xFD) fills it exactly
        assert_eq!(out, [7; 4]);
        assert!(unpack_bits(&[0xFD, 7], &mut out[..3]).is_err()); // ...and overruns 3
        assert!(unpack_bits(&[3, 1, 2], &mut out).is_err()); // literal of 4, only 2 bytes
        let mut out = [0u8; 4];
        unpack_bits(&[0xFF, 9, 1, 1, 2], &mut out).unwrap();
        assert_eq!(out, [9, 9, 1, 2]);
    }
}
