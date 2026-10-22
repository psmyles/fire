//! End-to-end tests for PSD layer stepping ([`PsdLayers`]): hand-built layered documents through
//! `open` and `decode_layer`, the path the viewer's layer jobs take.
//!
//! Like `psd.rs`, fixtures are written in code rather than committed: a layered PSD is the plain
//! header and sections plus a layer-and-mask section, and the builder below spells out every field
//! the parser relies on — layer records (bounds, channels, blend key, opacity, flags, mask block,
//! Pascal and `luni` names, `lsct` group markers), channel data in each of the four compressions,
//! and the `Lr16`/`Lr32` blocks 16- and 32-bit documents keep their layers in.
//!
//! The layer decoder is pure Rust, so most of this runs without the `psd` feature too. The tests
//! that compare a layer against psd_sdk's composite of the same pixels need the feature.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use fire_decode::{BlendMode, DecodeOptions, LayerCtl, LayerError, PixelFormat, PsdLayers, Rect};

const GRAY: u16 = 1;
const RGB: u16 = 3;
const CMYK: u16 = 4;
#[cfg_attr(not(feature = "psd"), allow(dead_code))] // only the composite comparison uses it
const LAB: u16 = 9;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Comp {
    Raw,
    Rle,
    Zip,
    ZipPred,
}

#[derive(Clone)]
struct Mask {
    rect: (i32, i32, i32, i32), // top, left, bottom, right
    default: u8,
    disabled: bool,
    /// Big-endian samples for the mask rect; empty = no channel data.
    data: Vec<u8>,
}

#[derive(Clone)]
struct L {
    name: String,
    luni: Option<String>,
    rect: (i32, i32, i32, i32),
    /// (channel id, big-endian samples covering `rect`)
    chans: Vec<(i16, Vec<u8>)>,
    comp: Comp,
    blend: [u8; 4],
    opacity: u8,
    clipping: u8,
    hidden: bool,
    lsct: Option<u32>,
    keys: Vec<[u8; 4]>,
    mask: Option<Mask>,
    /// A second ("real" user) mask; `mask` is then channel -2 and this one -3.
    mask2: Option<Mask>,
}

impl L {
    fn new(name: &str, rect: (i32, i32, i32, i32), chans: Vec<(i16, Vec<u8>)>) -> Self {
        Self {
            name: name.into(),
            luni: None,
            rect,
            chans,
            comp: Comp::Raw,
            blend: *b"norm",
            opacity: 255,
            clipping: 0,
            hidden: false,
            lsct: None,
            keys: Vec::new(),
            mask: None,
            mask2: None,
        }
    }

    fn group(name: &str, kind: u32) -> Self {
        let mut l = Self::new(name, (0, 0, 0, 0), Vec::new());
        l.lsct = Some(kind);
        l
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Placement {
    TopLevel,
    /// In a global `Lr16`/`Lr32` block, as Photoshop writes 16/32-bit documents.
    Deep,
}

struct Doc {
    w: u32,
    h: u32,
    depth: u16,
    mode: u16,
    /// Bottom-most first, as stored.
    layers: Vec<L>,
    placement: Placement,
    /// The merged image's planes (big-endian); zeros of the right size when `None`.
    composite: Option<Vec<Vec<u8>>>,
}

fn be16(v: &[u16]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_be_bytes()).collect()
}

fn be32f(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_be_bytes()).collect()
}

fn packbits_row(row: &[u8]) -> Vec<u8> {
    // Literals only, in chunks of up to 128, plus a run where 3+ bytes repeat: enough to
    // exercise both branches of the decoder.
    let mut out = Vec::new();
    let mut i = 0;
    while i < row.len() {
        let mut run = 1;
        while i + run < row.len() && run < 128 && row[i + run] == row[i] {
            run += 1;
        }
        if run >= 3 {
            out.push((257 - run) as u8);
            out.push(row[i]);
            i += run;
            continue;
        }
        let start = i;
        let mut n = 0;
        while i < row.len() && n < 128 {
            let mut r = 1;
            while i + r < row.len() && r < 3 && row[i + r] == row[i] {
                r += 1;
            }
            if r >= 3 {
                break;
            }
            i += 1;
            n += 1;
        }
        out.push((n - 1) as u8);
        out.extend_from_slice(&row[start..start + n]);
    }
    out
}

fn predict(data: &[u8], w: usize, h: usize, depth: u16) -> Vec<u8> {
    let mut out = data.to_vec();
    match depth {
        8 => {
            for row in out.chunks_mut(w).take(h) {
                for x in (1..row.len()).rev() {
                    row[x] = row[x].wrapping_sub(row[x - 1]);
                }
            }
        }
        16 => {
            for row in out.chunks_mut(w * 2).take(h) {
                let vals: Vec<u16> = row
                    .chunks(2)
                    .map(|c| u16::from_be_bytes([c[0], c[1]]))
                    .collect();
                for (x, px) in row.chunks_mut(2).enumerate() {
                    let d = if x == 0 {
                        vals[0]
                    } else {
                        vals[x].wrapping_sub(vals[x - 1])
                    };
                    px.copy_from_slice(&d.to_be_bytes());
                }
            }
        }
        _ => {
            for row in out.chunks_mut(w * 4).take(h) {
                let orig = row.to_vec();
                for x in 0..w {
                    for b in 0..4 {
                        row[b * w + x] = orig[x * 4 + b];
                    }
                }
                for x in (1..row.len()).rev() {
                    row[x] = row[x].wrapping_sub(row[x - 1]);
                }
            }
        }
    }
    out
}

fn encode(data: &[u8], w: usize, h: usize, depth: u16, comp: Comp) -> Vec<u8> {
    let bps = depth as usize / 8;
    let mut out = Vec::new();
    match comp {
        Comp::Raw => {
            out.extend_from_slice(&0u16.to_be_bytes());
            out.extend_from_slice(data);
        }
        Comp::Rle => {
            out.extend_from_slice(&1u16.to_be_bytes());
            let rows: Vec<Vec<u8>> = data.chunks(w * bps).map(packbits_row).collect();
            for r in &rows {
                out.extend_from_slice(&(r.len() as u16).to_be_bytes());
            }
            for r in &rows {
                out.extend_from_slice(r);
            }
        }
        Comp::Zip => {
            out.extend_from_slice(&2u16.to_be_bytes());
            out.extend(miniz_oxide::deflate::compress_to_vec_zlib(data, 6));
        }
        Comp::ZipPred => {
            out.extend_from_slice(&3u16.to_be_bytes());
            out.extend(miniz_oxide::deflate::compress_to_vec_zlib(
                &predict(data, w, h, depth),
                6,
            ));
        }
    }
    out
}

fn mask_block(m: &Mask, second: Option<&Mask>) -> Vec<u8> {
    let rect = |r: (i32, i32, i32, i32)| -> Vec<u8> {
        [r.0, r.1, r.2, r.3]
            .iter()
            .flat_map(|v| v.to_be_bytes())
            .collect()
    };
    let flags = |m: &Mask| if m.disabled { 2u8 } else { 0 };
    let mut b = rect(m.rect);
    b.push(m.default);
    b.push(flags(m) | if second.is_some() { 8 } else { 0 });
    match second {
        None => b.extend_from_slice(&[0, 0]), // pad to 20
        Some(s) => {
            b.push(flags(s));
            b.push(s.default);
            b.extend(rect(s.rect));
        }
    }
    b
}

fn layer_info_block(doc: &Doc) -> Vec<u8> {
    let mut recs = Vec::new();
    let mut data = Vec::new();
    for l in &doc.layers {
        let (t, le, bo, r) = l.rect;
        let (w, h) = (
            (r as i64 - le as i64).max(0) as usize,
            (bo as i64 - t as i64).max(0) as usize,
        );
        let mut chans: Vec<(i16, Vec<u8>)> = l
            .chans
            .iter()
            .map(|(id, d)| (*id, encode(d, w, h, doc.depth, l.comp)))
            .collect();
        for (k, m) in [&l.mask, &l.mask2].into_iter().enumerate() {
            if let Some(m) = m {
                let (mt, ml, mb, mr) = m.rect;
                let (mw, mh) = ((mr - ml).max(0) as usize, (mb - mt).max(0) as usize);
                let id = if k == 0 { -2 } else { -3 };
                let body = if m.data.is_empty() {
                    0u16.to_be_bytes().to_vec()
                } else {
                    encode(&m.data, mw, mh, doc.depth, l.comp)
                };
                chans.push((id, body));
            }
        }
        for v in [t, le, bo, r] {
            recs.extend_from_slice(&v.to_be_bytes());
        }
        recs.extend_from_slice(&(chans.len() as u16).to_be_bytes());
        for (id, body) in &chans {
            recs.extend_from_slice(&id.to_be_bytes());
            recs.extend_from_slice(&(body.len() as u32).to_be_bytes());
        }
        recs.extend_from_slice(b"8BIM");
        recs.extend_from_slice(&l.blend);
        recs.push(l.opacity);
        recs.push(l.clipping);
        recs.push(if l.hidden { 2 } else { 0 });
        recs.push(0);
        let mut extra = Vec::new();
        match &l.mask {
            Some(m) => {
                let mb = mask_block(m, l.mask2.as_ref());
                extra.extend_from_slice(&(mb.len() as u32).to_be_bytes());
                extra.extend(mb);
            }
            None => extra.extend_from_slice(&0u32.to_be_bytes()),
        }
        extra.extend_from_slice(&0u32.to_be_bytes()); // blending ranges
                                                      // Pascal name: length byte + bytes, the whole field padded to a multiple of 4.
        let name = l.name.as_bytes();
        extra.push(name.len() as u8);
        extra.extend_from_slice(name);
        extra.resize(
            extra.len() + (name.len() + 1).div_ceil(4) * 4 - (name.len() + 1),
            0,
        );
        let mut add = |key: &[u8; 4], d: &[u8]| {
            extra.extend_from_slice(b"8BIM");
            extra.extend_from_slice(key);
            extra.extend_from_slice(&(d.len() as u32).to_be_bytes());
            extra.extend_from_slice(d);
            if d.len() % 2 == 1 {
                extra.push(0);
            }
        };
        if let Some(u) = &l.luni {
            let units: Vec<u16> = u.encode_utf16().collect();
            let mut d = (units.len() as u32).to_be_bytes().to_vec();
            d.extend(be16(&units));
            add(b"luni", &d);
        }
        if let Some(k) = l.lsct {
            add(b"lsct", &k.to_be_bytes());
        }
        for k in &l.keys {
            add(k, &[0, 0, 0, 0]);
        }
        recs.extend_from_slice(&(extra.len() as u32).to_be_bytes());
        recs.extend(extra);
        for (_, body) in chans {
            data.extend(body);
        }
    }
    let mut b = (doc.layers.len() as i16).to_be_bytes().to_vec();
    b.extend(recs);
    b.extend(data);
    b
}

fn build(doc: &Doc) -> Vec<u8> {
    let colour = match doc.mode {
        GRAY => 1,
        CMYK => 4,
        _ => 3,
    };
    let bps = doc.depth as usize / 8;
    let composite = doc
        .composite
        .clone()
        .unwrap_or_else(|| vec![vec![0u8; doc.w as usize * doc.h as usize * bps]; colour]);
    let mut b = Vec::new();
    b.extend_from_slice(b"8BPS");
    b.extend_from_slice(&1u16.to_be_bytes());
    b.extend_from_slice(&[0; 6]);
    b.extend_from_slice(&(composite.len() as u16).to_be_bytes());
    b.extend_from_slice(&doc.h.to_be_bytes());
    b.extend_from_slice(&doc.w.to_be_bytes());
    b.extend_from_slice(&doc.depth.to_be_bytes());
    b.extend_from_slice(&doc.mode.to_be_bytes());
    b.extend_from_slice(&0u32.to_be_bytes()); // colour mode data
    b.extend_from_slice(&0u32.to_be_bytes()); // image resources

    let info = layer_info_block(doc);
    let mut lm = Vec::new();
    match doc.placement {
        Placement::TopLevel => {
            let mut padded = info.clone();
            if padded.len() % 2 == 1 {
                padded.push(0);
            }
            lm.extend_from_slice(&(padded.len() as u32).to_be_bytes());
            lm.extend(padded);
            lm.extend_from_slice(&0u32.to_be_bytes()); // global mask info
        }
        Placement::Deep => {
            lm.extend_from_slice(&0u32.to_be_bytes()); // empty top-level layer info
            lm.extend_from_slice(&0u32.to_be_bytes()); // global mask info
            let key = if doc.depth == 16 { b"Lr16" } else { b"Lr32" };
            let mut padded = info.clone();
            while !padded.len().is_multiple_of(4) {
                padded.push(0);
            }
            lm.extend_from_slice(b"8BIM");
            lm.extend_from_slice(key);
            lm.extend_from_slice(&(padded.len() as u32).to_be_bytes());
            lm.extend(padded);
        }
    }
    b.extend_from_slice(&(lm.len() as u32).to_be_bytes());
    b.extend(lm);
    b.extend_from_slice(&0u16.to_be_bytes());
    for p in composite {
        b.extend(p);
    }
    b
}

fn open(doc: &Doc) -> PsdLayers {
    PsdLayers::open(Arc::new(build(doc)))
        .expect("parses")
        .expect("has layers")
}

fn decode(layers: &PsdLayers, i: usize) -> fire_decode::LayerImage {
    layers
        .decode_layer(i, &DecodeOptions::default(), &LayerCtl::DEFAULT)
        .expect("decodes")
}

fn rect(t: i32, l: i32, b: i32, r: i32) -> (i32, i32, i32, i32) {
    (t, l, b, r)
}

/// Deterministic pseudo-random bytes.
fn noise(n: usize, seed: u64) -> Vec<u8> {
    let mut s = seed.wrapping_mul(2654435761).wrapping_add(1);
    (0..n)
        .map(|_| {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (s >> 56) as u8
        })
        .collect()
}

/// The basic shape: order, names, group paths, visibility, blend/opacity, and exact pixels at the
/// right canvas position.
#[test]
fn rgba8_layers_in_a_group() {
    let bg = L::new(
        "Background",
        rect(0, 0, 3, 4),
        vec![(0, vec![10; 12]), (1, vec![20; 12]), (2, vec![30; 12])],
    );
    let mut child = L::new(
        "child",
        rect(1, 1, 3, 3), // 2x2 at (1,1)
        vec![
            (-1, vec![255, 128, 64, 0]),
            (0, vec![1, 2, 3, 4]),
            (1, vec![5, 6, 7, 8]),
            (2, vec![9, 10, 11, 12]),
        ],
    );
    child.luni = Some("Child ✓".into());
    child.blend = *b"mul ";
    child.opacity = 128;
    let mut hidden = L::new(
        "hidden child",
        rect(0, 0, 1, 1),
        vec![(0, vec![1]), (1, vec![1]), (2, vec![1])],
    );
    hidden.hidden = true;
    let mut folder = L::group("Group", 1);
    folder.hidden = false;
    let top = L::new(
        "top",
        rect(0, 3, 1, 4),
        vec![(0, vec![7]), (1, vec![8]), (2, vec![9])],
    );
    let doc = Doc {
        w: 4,
        h: 3,
        depth: 8,
        mode: RGB,
        layers: vec![
            bg,
            L::group("</Layer group>", 3),
            child,
            hidden,
            folder,
            top,
        ],
        placement: Placement::TopLevel,
        composite: None,
    };
    let layers = open(&doc);
    let names: Vec<_> = layers.layers().iter().map(|l| l.name.as_str()).collect();
    assert_eq!(names, ["top", "hidden child", "Child ✓", "Background"]);
    let child = &layers.layers()[2];
    assert_eq!(child.group_path, ["Group"]);
    assert_eq!(child.blend, BlendMode::Multiply);
    assert_eq!(child.opacity, 128);
    assert!(layers.layers()[1].hidden);
    assert!(!child.hidden && !child.hidden_by_group);
    assert!(layers.layers()[0].group_path.is_empty());

    let img = decode(&layers, 2);
    assert_eq!(img.canvas, (4, 3));
    assert_eq!(img.origin, (1, 1));
    assert_eq!((img.image.width, img.image.height), (2, 2));
    assert_eq!(img.image.format, PixelFormat::Rgba8Unorm);
    assert_eq!(img.image.channels, 4);
    assert!(!img.image.alpha_opaque);
    assert_eq!(
        img.image.pixels,
        vec![1, 5, 9, 255, 2, 6, 10, 128, 3, 7, 11, 64, 4, 8, 12, 0]
    );

    // A full-canvas layer with no transparency channel is opaque, and says so.
    let bg = decode(&layers, 3);
    assert_eq!(bg.origin, (0, 0));
    assert!(bg.image.alpha_opaque);
    assert_eq!(&bg.image.pixels[..4], &[10, 20, 30, 255]);
}

/// RLE, ZIP and ZIP-with-prediction decode to exactly what RAW does, at every depth, both at the
/// top level and inside `Lr16`/`Lr32`.
#[test]
fn every_compression_matches_raw_at_every_depth() {
    let (w, h) = (37usize, 11usize);
    for depth in [8u16, 16, 32] {
        let bps = depth as usize / 8;
        let plane = |seed| -> Vec<u8> {
            match depth {
                32 => be32f(
                    &noise(w * h, seed)
                        .iter()
                        .map(|&b| b as f32 / 64.0)
                        .collect::<Vec<_>>(),
                ),
                // Long runs of equal samples so RLE has runs to find.
                _ => noise(w * h * bps, seed)
                    .chunks(8)
                    .flat_map(|c| std::iter::repeat_n(c[0], c.len()))
                    .collect(),
            }
        };
        let chans = vec![(-1, plane(1)), (0, plane(2)), (1, plane(3)), (2, plane(4))];
        let mut reference = None;
        for comp in [Comp::Raw, Comp::Rle, Comp::Zip, Comp::ZipPred] {
            if depth == 32 && comp == Comp::Zip {
                continue; // 32-bit ZIP is always predicted (psd_sdk and Photoshop agree)
            }
            for placement in [Placement::TopLevel, Placement::Deep] {
                if depth == 8 && placement == Placement::Deep {
                    continue;
                }
                let mut l = L::new("x", rect(2, 3, 2 + h as i32, 3 + w as i32), chans.clone());
                l.comp = comp;
                let doc = Doc {
                    w: 64,
                    h: 32,
                    depth,
                    mode: RGB,
                    layers: vec![l],
                    placement,
                    composite: None,
                };
                let img = decode(&open(&doc), 0);
                assert_eq!(img.origin, (3, 2));
                let expect = match depth {
                    8 => PixelFormat::Rgba8Unorm,
                    16 => PixelFormat::Rgba16Unorm,
                    _ => PixelFormat::Rgba32Float,
                };
                assert_eq!(img.image.format, expect);
                match &reference {
                    None => reference = Some(img.image.pixels),
                    Some(r) => assert!(
                        *r == img.image.pixels,
                        "{comp:?} at {depth}-bit differs from RAW"
                    ),
                }
            }
        }
        // And RAW itself put the samples where they belong.
        let px = reference.unwrap();
        match depth {
            8 => assert_eq!(
                &px[..4],
                &[chans[1].1[0], chans[2].1[0], chans[3].1[0], chans[0].1[0]]
            ),
            16 => {
                let r = u16::from_ne_bytes([px[0], px[1]]);
                assert_eq!(r, u16::from_be_bytes([chans[1].1[0], chans[1].1[1]]));
            }
            _ => {
                let r = f32::from_ne_bytes(px[0..4].try_into().unwrap());
                assert_eq!(r, f32::from_be_bytes(chans[1].1[0..4].try_into().unwrap()));
            }
        }
    }
}

/// Masks multiply alpha: a mask smaller than the layer uses its default colour outside its rect;
/// a disabled mask is ignored; with two masks both apply.
#[test]
fn masks_multiply_alpha() {
    let rgb = |n| vec![(0, vec![200u8; n]), (1, vec![100; n]), (2, vec![50; n])];
    let doc_with = |mask: Option<Mask>, mask2: Option<Mask>| {
        let mut l = L::new("m", rect(0, 0, 1, 4), rgb(4));
        l.mask = mask;
        l.mask2 = mask2;
        Doc {
            w: 4,
            h: 1,
            depth: 8,
            mode: RGB,
            layers: vec![l],
            placement: Placement::TopLevel,
            composite: None,
        }
    };
    let alpha = |doc: &Doc| -> Vec<u8> {
        decode(&open(doc), 0)
            .image
            .pixels
            .chunks(4)
            .map(|p| p[3])
            .collect()
    };
    // 2-px mask at x=1..3, default 0 outside.
    let m = Mask {
        rect: rect(0, 1, 1, 3),
        default: 0,
        disabled: false,
        data: vec![255, 51],
    };
    assert_eq!(alpha(&doc_with(Some(m.clone()), None)), [0, 255, 51, 0]);
    // Default 255 outside.
    let m255 = Mask {
        default: 255,
        ..m.clone()
    };
    assert_eq!(
        alpha(&doc_with(Some(m255.clone()), None)),
        [255, 255, 51, 255]
    );
    // Disabled: no effect.
    let off = Mask {
        disabled: true,
        ..m.clone()
    };
    assert_eq!(alpha(&doc_with(Some(off), None)), [255; 4]);
    // Both masks multiply.
    let half = Mask {
        rect: rect(0, 0, 1, 4),
        default: 255,
        disabled: false,
        data: vec![128, 128, 128, 128],
    };
    assert_eq!(
        alpha(&doc_with(Some(m255), Some(half))),
        [128, 128, 26, 128]
    );
    // A mask with no pixel data is its default everywhere.
    let blank = Mask {
        rect: rect(0, 0, 0, 0),
        default: 0,
        disabled: false,
        data: Vec::new(),
    };
    assert_eq!(alpha(&doc_with(Some(blank), None)), [0; 4]);
}

/// A layer hanging off every edge of the canvas is cropped to it.
#[test]
fn layer_off_the_canvas_edges_is_cropped() {
    // 6x4 layer at (-2,-1) on a 3x2 canvas: the on-canvas part is columns 2..5, rows 1..3.
    let (w, h) = (6usize, 4usize);
    let r: Vec<u8> = (0..(w * h) as u8).collect();
    let l = L::new(
        "big",
        rect(-1, -2, 3, 4),
        vec![(0, r.clone()), (1, r.clone()), (2, r)],
    );
    let doc = Doc {
        w: 3,
        h: 2,
        depth: 8,
        mode: RGB,
        layers: vec![l],
        placement: Placement::TopLevel,
        composite: None,
    };
    let layers = open(&doc);
    assert_eq!(
        layers.layers()[0].bounds,
        Rect {
            left: -2,
            top: -1,
            right: 4,
            bottom: 3
        }
    );
    let img = decode(&layers, 0);
    assert_eq!(img.origin, (0, 0));
    assert_eq!((img.image.width, img.image.height), (3, 2));
    let reds: Vec<u8> = img.image.pixels.chunks(4).map(|p| p[0]).collect();
    assert_eq!(reds, [8, 9, 10, 14, 15, 16]);
    assert!(img.image.alpha_opaque, "covers the whole canvas, opaque");
}

/// Groups, dividers, adjustment layers and empty layers are not steppable; a file with nothing
/// else has no layers to offer.
#[test]
fn layers_without_pixels_are_not_steppable() {
    let mut adj = L::new("Levels 1", rect(0, 0, 2, 2), Vec::new());
    adj.keys.push(*b"levl");
    let empty = L::new(
        "Layer 1",
        rect(0, 0, 0, 0),
        vec![(0, vec![]), (1, vec![]), (2, vec![])],
    );
    let doc = Doc {
        w: 2,
        h: 2,
        depth: 8,
        mode: RGB,
        layers: vec![L::group("</g>", 3), adj, empty, L::group("G", 2)],
        placement: Placement::TopLevel,
        composite: None,
    };
    assert!(PsdLayers::open(Arc::new(build(&doc))).unwrap().is_none());
}

/// Greyscale, CMYK and Lab layers convert to RGB exactly as psd_sdk's composite does.
#[cfg(feature = "psd")]
#[test]
fn colour_modes_match_the_composite() {
    for (mode, planes) in [(GRAY, 1usize), (CMYK, 4), (LAB, 3), (RGB, 3)] {
        for depth in [8u16, 16] {
            let (w, h) = (9usize, 5usize);
            let bps = depth as usize / 8;
            let data: Vec<Vec<u8>> = (0..planes)
                .map(|p| noise(w * h * bps, 40 + p as u64))
                .collect();
            let chans = data
                .iter()
                .enumerate()
                .map(|(i, d)| (i as i16, d.clone()))
                .collect();
            let doc = Doc {
                w: w as u32,
                h: h as u32,
                depth,
                mode,
                layers: vec![L::new("only", rect(0, 0, h as i32, w as i32), chans)],
                placement: if depth == 8 {
                    Placement::TopLevel
                } else {
                    Placement::Deep
                },
                composite: Some(data),
            };
            let bytes = build(&doc);
            let composite =
                fire_decode::decode(&bytes, Some("psd"), &DecodeOptions::default()).unwrap();
            let layer = decode(&PsdLayers::open(Arc::new(bytes)).unwrap().unwrap(), 0);
            assert_eq!(
                composite.pixels, layer.image.pixels,
                "mode {mode} at {depth}-bit"
            );
            assert_eq!(layer.image.channels, if mode == GRAY { 2 } else { 4 });
        }
    }
}

/// With a canvas over `max_dim`, a full-canvas layer downscales to exactly the composite's pixels,
/// and a partial one to the matching sub-rectangle of them.
#[cfg(feature = "psd")]
#[test]
fn downscaled_layers_line_up_with_the_downscaled_composite() {
    let (w, h) = (41usize, 29usize);
    let data: Vec<Vec<u8>> = (0..3).map(|p| noise(w * h, 70 + p)).collect();
    let full = L::new(
        "full",
        rect(0, 0, h as i32, w as i32),
        data.iter()
            .enumerate()
            .map(|(i, d)| (i as i16, d.clone()))
            .collect(),
    );
    let crop = |d: &Vec<u8>| -> Vec<u8> {
        (5..20)
            .flat_map(|y| d[y * w + 7..y * w + 30].to_vec())
            .collect()
    };
    let part = L::new(
        "part",
        rect(5, 7, 20, 30),
        data.iter()
            .enumerate()
            .map(|(i, d)| (i as i16, crop(d)))
            .collect(),
    );
    let doc = Doc {
        w: w as u32,
        h: h as u32,
        depth: 8,
        mode: RGB,
        layers: vec![full, part],
        placement: Placement::TopLevel,
        composite: Some(data),
    };
    let bytes = build(&doc);
    let opts = DecodeOptions {
        max_dim: 16,
        ..DecodeOptions::default()
    };
    let composite = fire_decode::decode(&bytes, Some("psd"), &opts).unwrap();
    let layers = PsdLayers::open(Arc::new(bytes)).unwrap().unwrap();
    let ctl = LayerCtl::DEFAULT;
    let full = layers.decode_layer(1, &opts, &ctl).unwrap();
    assert_eq!(full.canvas, (composite.width, composite.height));
    assert_eq!(full.image.pixels, composite.pixels);
    let part = layers.decode_layer(0, &opts, &ctl).unwrap();
    let (ox, oy) = part.origin;
    for y in 0..part.image.height {
        for x in 0..part.image.width {
            let d = ((y * part.image.width + x) * 4) as usize;
            let s = (((oy + y) * composite.width + ox + x) * 4) as usize;
            assert_eq!(part.image.pixels[d..d + 3], composite.pixels[s..s + 3]);
        }
    }
}

/// One `PsdLayers` serves many threads at once, all getting identical results.
#[test]
fn concurrent_decodes_of_one_layer_agree() {
    let (w, h) = (300usize, 200usize);
    let mut l = L::new(
        "x",
        rect(0, 0, h as i32, w as i32),
        (0..3).map(|c| (c as i16, noise(w * h, c))).collect(),
    );
    l.comp = Comp::Rle;
    let doc = Doc {
        w: w as u32,
        h: h as u32,
        depth: 8,
        mode: RGB,
        layers: vec![l],
        placement: Placement::TopLevel,
        composite: None,
    };
    let layers = Arc::new(open(&doc));
    let reference = decode(&layers, 0).image.pixels;
    std::thread::scope(|s| {
        for _ in 0..8 {
            let layers = Arc::clone(&layers);
            let reference = &reference;
            s.spawn(move || assert_eq!(decode(&layers, 0).image.pixels, *reference));
        }
    });
}

/// The cancel callback stops a decode, which reports `Cancelled` rather than a partial image.
#[test]
fn cancelled_decode_reports_cancelled() {
    let (w, h) = (256usize, 256usize);
    let mut l = L::new(
        "x",
        rect(0, 0, h as i32, w as i32),
        (0..3).map(|c| (c as i16, noise(w * h, c))).collect(),
    );
    l.comp = Comp::Zip;
    let doc = Doc {
        w: w as u32,
        h: h as u32,
        depth: 8,
        mode: RGB,
        layers: vec![l],
        placement: Placement::TopLevel,
        composite: None,
    };
    let layers = open(&doc);
    let calls = AtomicUsize::new(0);
    let cancel = || calls.fetch_add(1, Ordering::Relaxed) >= 2;
    let one = || 1;
    let ctl = LayerCtl {
        threads: &one,
        cancel: &cancel,
    };
    assert!(matches!(
        layers.decode_layer(0, &DecodeOptions::default(), &ctl),
        Err(LayerError::Cancelled)
    ));
}

// --- malformed input: an error or "no layers", never a panic or an abort ----------------------

fn rgb_layer(name: &str) -> L {
    L::new(
        name,
        rect(0, 0, 2, 2),
        vec![(0, vec![1; 4]), (1, vec![2; 4]), (2, vec![3; 4])],
    )
}

fn doc_of(layers: Vec<L>) -> Doc {
    Doc {
        w: 2,
        h: 2,
        depth: 8,
        mode: RGB,
        layers,
        placement: Placement::TopLevel,
        composite: None,
    }
}

/// Find the first byte offset of `needle` in `hay`.
fn find(hay: &[u8], needle: &[u8]) -> usize {
    hay.windows(needle.len())
        .position(|w| w == needle)
        .expect("needle present")
}

#[test]
fn malformed_channel_data_is_an_error() {
    // Unknown compression code.
    let mut bytes = build(&doc_of(vec![rgb_layer("a")]));
    let records_end = find(&bytes, b"\x01a"); // Pascal name "a"
    let data = records_end + 4; // name field is 4 bytes; channel data follows the only record
    bytes[data..data + 2].copy_from_slice(&7u16.to_be_bytes());
    let layers = PsdLayers::open(Arc::new(bytes)).unwrap().unwrap();
    assert!(matches!(
        layers.decode_layer(0, &DecodeOptions::default(), &LayerCtl::DEFAULT),
        Err(LayerError::Decode(_))
    ));

    // A ZIP stream that is garbage.
    let mut l = rgb_layer("z");
    l.comp = Comp::Zip;
    let mut bytes = build(&doc_of(vec![l]));
    let data = find(&bytes, b"\x01z") + 4;
    for b in &mut bytes[data + 2..data + 8] {
        *b ^= 0x5A;
    }
    let layers = PsdLayers::open(Arc::new(bytes)).unwrap().unwrap();
    assert!(layers
        .decode_layer(0, &DecodeOptions::default(), &LayerCtl::DEFAULT)
        .is_err());

    // An RLE row whose run overruns it.
    let mut l = rgb_layer("r");
    l.comp = Comp::Rle;
    let mut bytes = build(&doc_of(vec![l]));
    let data = find(&bytes, b"\x01r") + 4;
    // compression(2) + 2 row counts(4) then row 0: a run byte; make it a 100-byte run.
    bytes[data + 6] = (257 - 100) as u8;
    let layers = PsdLayers::open(Arc::new(bytes)).unwrap().unwrap();
    assert!(layers
        .decode_layer(0, &DecodeOptions::default(), &LayerCtl::DEFAULT)
        .is_err());
}

/// A channel that holds only its compression word has no pixels: it is the channel's default
/// everywhere, not a short channel.
#[test]
fn two_byte_channel_is_its_default() {
    for comp in [Comp::Raw, Comp::Rle] {
        let mut l = rgb_layer("a");
        l.chans.push((-1, Vec::new())); // transparency: encodes to the compression word alone
        l.comp = comp;
        let layers = open(&doc_of(vec![l]));
        let img = decode(&layers, 0);
        assert_eq!(img.image.pixels, [1, 2, 3, 0].repeat(4), "{comp:?}");
    }
}

#[test]
fn malformed_records_are_refused_at_open() {
    // A channel length pointing past the end of the file.
    let mut bytes = build(&doc_of(vec![rgb_layer("a")]));
    let first_chan_len = 16 + 2 + 2; // rect, channel count, first channel id
    let at = find(&bytes, &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 2]) + first_chan_len;
    bytes[at..at + 4].copy_from_slice(&0x7fff_ffffu32.to_be_bytes());
    assert!(PsdLayers::open(Arc::new(bytes)).is_err());

    // A corrupt record signature.
    let mut bytes = build(&doc_of(vec![rgb_layer("a")]));
    let at = find(&bytes, b"8BIMnorm");
    bytes[at] = b'X';
    assert!(PsdLayers::open(Arc::new(bytes)).is_err());

    // A truncated file: every prefix either errors or yields no layers — never panics.
    let bytes = build(&doc_of(vec![rgb_layer("a"), rgb_layer("b")]));
    for n in 0..bytes.len() {
        let _ = PsdLayers::open(Arc::new(bytes[..n].to_vec()));
    }
}

/// Bounds of `i32::MIN..i32::MAX` are cropped to the canvas and then fail cleanly on their (far
/// too short) data instead of overflowing anything.
#[test]
fn extreme_bounds_are_handled() {
    let mut l = rgb_layer("huge");
    l.rect = (i32::MIN, i32::MIN, i32::MAX, i32::MAX);
    let layers = PsdLayers::open(Arc::new(build(&doc_of(vec![l]))))
        .unwrap()
        .unwrap();
    assert_eq!(layers.layers()[0].clip.width(), 2);
    assert!(layers
        .decode_layer(0, &DecodeOptions::default(), &LayerCtl::DEFAULT)
        .is_err());
}

/// A `luni` block claiming four billion characters, and 300 nested groups.
#[test]
fn hostile_metadata_is_tolerated() {
    let mut l = rgb_layer("n");
    l.luni = Some("ok".into());
    let mut bytes = build(&doc_of(vec![l]));
    let at = find(&bytes, b"luni") + 8;
    bytes[at..at + 4].copy_from_slice(&u32::MAX.to_be_bytes());
    let layers = PsdLayers::open(Arc::new(bytes)).unwrap().unwrap();
    assert_eq!(layers.layers().len(), 1);

    let mut stack = Vec::new();
    for _ in 0..300 {
        stack.push(L::group("</g>", 3));
    }
    stack.push(rgb_layer("deep"));
    for i in 0..300 {
        stack.push(L::group(&format!("g{i}"), 1));
    }
    let layers = open(&doc_of(stack));
    assert_eq!(layers.layers().len(), 1);
    assert!(layers.layers()[0].group_path.len() <= 300);
}

/// The real thing, when one is available: every layer of the PSD named by `FIRE_PSD_SAMPLE`
/// decodes, with timings. `cargo test --release -p fire-decode --test psd_layers -- --ignored
/// --nocapture` with the variable set.
#[test]
#[ignore]
fn sample_file_decodes_every_layer() {
    let Ok(path) = std::env::var("FIRE_PSD_SAMPLE") else {
        return;
    };
    let t = std::time::Instant::now();
    let bytes = Arc::new(std::fs::read(&path).unwrap());
    let read = t.elapsed();
    let t = std::time::Instant::now();
    let layers = PsdLayers::open(bytes).unwrap().expect("has layers");
    println!(
        "read {:.1} ms, index {:.2} ms, {} layers, canvas {:?}",
        read.as_secs_f64() * 1e3,
        t.elapsed().as_secs_f64() * 1e3,
        layers.layers().len(),
        layers.canvas()
    );
    for (i, l) in layers.layers().iter().enumerate() {
        for (label, threads) in [("all", 0usize), ("1t", 1)] {
            let th = move || threads;
            let ctl = LayerCtl {
                threads: &th,
                cancel: &|| false,
            };
            let t = std::time::Instant::now();
            let img = layers
                .decode_layer(i, &DecodeOptions::default(), &ctl)
                .unwrap();
            println!(
                "{i:2} {:30} {label:>3} {:7.1} ms  {}x{} at {:?}",
                l.name,
                t.elapsed().as_secs_f64() * 1e3,
                img.image.width,
                img.image.height,
                img.origin
            );
        }
    }
}
