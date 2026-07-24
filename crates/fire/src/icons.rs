//! Toolbar icons: build-time-rasterized SVG coverage masks, packed into one GPU atlas.
//!
//! `build.rs` rasterizes each `assets/icons/*.svg` to a square **A8 coverage mask** at [`MASTER`]²
//! px (white-on-transparent → the alpha channel is the coverage) and drops it in `OUT_DIR`; we embed
//! those masters via `include_bytes!`. At runtime [`atlas`] downsamples every master to the exact
//! physical icon size for the current DPI and packs them side by side into a single RGBA8 strip —
//! **white RGB, coverage in alpha** — which [`crate::render::imgui`] uploads as one D3D11 texture.
//!
//! White-with-alpha is what lets a single texture serve every tint: ImGui's pixel shader multiplies
//! the sampled texel by the vertex color, so `(1,1,1,a) * tint` yields the tint at the mask's
//! coverage. One texture, any color, no per-tint CPU work — which is why the old GDI path (tint into
//! a scratch DIB, then `AlphaBlend`, once per icon per repaint) is gone.
//!
//! Off the time-to-first-pixel path: the masters live in `.rodata`, and the downsample+pack runs
//! once per DPI change, never per frame.

/// Master rasterization size of each embedded icon (px). Must match `build.rs`'s `ICON_MASTER`;
/// each `.a8` master is exactly `MASTER * MASTER` bytes.
const MASTER: usize = 64;

/// A toolbar icon. The order matches `build.rs`'s `ICON_STEMS` (and [`MASTERS`]) so `icon as usize`
/// indexes both the embedded master and the icon's cell in the atlas strip. Several buttons share an
/// icon (e.g. the blue-channel and black-background buttons both use [`Icon::B`]); the UI maps each
/// action to one of these.
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Left,
    Right,
    ZoomOut,
    ZoomIn,
    Fit,
    OneToOne,
    Rgb,
    Rgba,
    R,
    G,
    B,
    A,
    Aces,
    EvUp,
    EvReset,
    EvDown,
    White,
    Checker,
    Outline,
    OpenWith,
    Fullscreen,
    Flipbook,
    Play,
    Pause,
    More,
    Octagon,
}

/// Declare the icon order once: the embedded masters and their file stems are generated from the
/// same list, so the `include_bytes!` paths cannot drift out of order against the names a test can
/// check. Keeping them as two hand-written lists is what would let a reorder pass silently.
macro_rules! icon_masters {
    ($($stem:literal),+ $(,)?) => {
        /// The embedded A8 masters, indexed by `Icon as usize`. Same order as the enum / `build.rs`.
        const MASTERS: &[&[u8]] = &[
            $(include_bytes!(concat!(env!("OUT_DIR"), "/", $stem, ".a8"))),+
        ];
        /// The same stems as names rather than bytes: what makes the order above checkable
        /// against `build.rs`. Nothing at runtime needs them, only the order test.
        #[cfg(test)]
        const STEMS: &[&str] = &[$($stem),+];
    };
}

icon_masters![
    "icon_left",
    "icon_right",
    "icon_zoom_out",
    "icon_zoom_in",
    "icon_fit",
    "icon_1_1",
    "icon_RGB",
    "icon_rgba",
    "icon_R",
    "icon_G",
    "icon_B",
    "icon_A",
    "icon_aces",
    "icon_ev+",
    "icon_ev0",
    "icon_ev-",
    "icon_W",
    "icon_C",
    "icon_outline",
    "icon_open_with",
    "icon_fullscreen",
    "icon_flipbook",
    "icon_play",
    "icon_pause",
    "icon_more",
    "icon_octagon",
];

/// Number of icons in the atlas strip.
pub const COUNT: usize = MASTERS.len();

/// Compile-time guard that [`Icon`] and [`MASTERS`] stay the same length: [`Icon::uv`] and [`atlas`]
/// index by `icon as usize`, so a variant added without a corresponding master (or vice-versa) must
/// fail the build rather than panic at runtime. `Octagon` must stay the last `Icon` variant for this
/// check to hold. Length is all this can prove — that the *order* also lines up is
/// `icon_order_matches_the_build_script`'s job.
const _: () = assert!(Icon::Octagon as usize + 1 == COUNT);

impl Icon {
    /// This icon's UV rect within the atlas strip (icons are packed left-to-right, one row).
    pub fn uv(self) -> ([f32; 2], [f32; 2]) {
        let i = self as usize as f32;
        let n = COUNT as f32;
        ([i / n, 0.0], [(i + 1.0) / n, 1.0])
    }

    /// The SVG file stem this variant is rasterized from. Exhaustive on purpose: a new variant
    /// does not compile until it names its icon, and the test below proves the name it gives
    /// lands at the same index as the master it will be drawn from.
    #[cfg(test)]
    fn stem(self) -> &'static str {
        match self {
            Icon::Left => "icon_left",
            Icon::Right => "icon_right",
            Icon::ZoomOut => "icon_zoom_out",
            Icon::ZoomIn => "icon_zoom_in",
            Icon::Fit => "icon_fit",
            Icon::OneToOne => "icon_1_1",
            Icon::Rgb => "icon_RGB",
            Icon::Rgba => "icon_rgba",
            Icon::R => "icon_R",
            Icon::G => "icon_G",
            Icon::B => "icon_B",
            Icon::A => "icon_A",
            Icon::Aces => "icon_aces",
            Icon::EvUp => "icon_ev+",
            Icon::EvReset => "icon_ev0",
            Icon::EvDown => "icon_ev-",
            Icon::White => "icon_W",
            Icon::Checker => "icon_C",
            Icon::Outline => "icon_outline",
            Icon::OpenWith => "icon_open_with",
            Icon::Fullscreen => "icon_fullscreen",
            Icon::Flipbook => "icon_flipbook",
            Icon::Play => "icon_play",
            Icon::Pause => "icon_pause",
            Icon::More => "icon_more",
            Icon::Octagon => "icon_octagon",
        }
    }
}

/// Every [`Icon`] variant, in declaration order — the list the order test walks.
#[cfg(test)]
const ALL_ICONS: [Icon; COUNT] = [
    Icon::Left,
    Icon::Right,
    Icon::ZoomOut,
    Icon::ZoomIn,
    Icon::Fit,
    Icon::OneToOne,
    Icon::Rgb,
    Icon::Rgba,
    Icon::R,
    Icon::G,
    Icon::B,
    Icon::A,
    Icon::Aces,
    Icon::EvUp,
    Icon::EvReset,
    Icon::EvDown,
    Icon::White,
    Icon::Checker,
    Icon::Outline,
    Icon::OpenWith,
    Icon::Fullscreen,
    Icon::Flipbook,
    Icon::Play,
    Icon::Pause,
    Icon::More,
    Icon::Octagon,
];

/// Build the RGBA8 atlas strip for a physical icon edge of `icon_px`: `COUNT * icon_px` wide,
/// `icon_px` tall, every texel white with the mask's coverage in alpha. Returns the pixels and the
/// strip's width in px (height is `icon_px`).
pub fn atlas(icon_px: usize) -> (Vec<u8>, usize) {
    let n = icon_px.max(1);
    let w = n * COUNT;
    let mut px = vec![0u8; w * n * 4];
    for (i, master) in MASTERS.iter().enumerate() {
        let mask = downsample(master, n);
        let x0 = i * n;
        for y in 0..n {
            for x in 0..n {
                let o = (y * w + x0 + x) * 4;
                px[o] = 255;
                px[o + 1] = 255;
                px[o + 2] = 255;
                px[o + 3] = mask[y * n + x];
            }
        }
    }
    (px, w)
}

/// Box-downsample a `MASTER²` coverage mask to `dst²` by averaging each destination pixel's source
/// footprint — effectively supersampled antialiasing of the master raster down to the icon size.
/// (If `dst > MASTER` the footprints collapse to single texels, i.e. nearest-neighbor upsample;
/// only the very highest DPIs reach that, and the master is sized so it rarely happens.)
fn downsample(master: &[u8], dst: usize) -> Vec<u8> {
    debug_assert_eq!(master.len(), MASTER * MASTER);
    let mut out = vec![0u8; dst * dst];
    let scale = MASTER as f32 / dst as f32;
    for ty in 0..dst {
        let sy0 = (ty as f32 * scale) as usize;
        let sy1 = (((ty + 1) as f32 * scale).ceil() as usize).clamp(sy0 + 1, MASTER);
        for tx in 0..dst {
            let sx0 = (tx as f32 * scale) as usize;
            let sx1 = (((tx + 1) as f32 * scale).ceil() as usize).clamp(sx0 + 1, MASTER);
            let mut sum = 0u32;
            let mut count = 0u32;
            for sy in sy0..sy1 {
                for sx in sx0..sx1 {
                    sum += master[sy * MASTER + sx] as u32;
                    count += 1;
                }
            }
            out[ty * dst + tx] = (sum / count.max(1)) as u8;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atlas_is_a_strip_of_square_cells() {
        let (px, w) = atlas(8);
        assert_eq!(w, 8 * COUNT);
        assert_eq!(px.len(), w * 8 * 4);
        // Every texel is white; only alpha carries the shape (that's what makes one texture tintable).
        assert!(px
            .chunks_exact(4)
            .all(|t| t[0] == 255 && t[1] == 255 && t[2] == 255));
        // ...and at least one texel is actually opaque, i.e. we packed real coverage, not a blank.
        assert!(px.chunks_exact(4).any(|t| t[3] > 0));
    }

    /// The one drift the compile-time length guard cannot catch: a *reorder*. `Icon as usize`
    /// indexes both [`MASTERS`] and the atlas cell, so if the enum and the stem list disagree on
    /// order, every button after the mismatch draws the wrong glyph — with no build failure and
    /// nothing to see in a diff. Pin both halves against `build.rs`, which owns the rasterization
    /// order, the same way `fire-decode` pins the extension table against the installer script.
    #[test]
    fn icon_order_matches_the_build_script() {
        // `Icon as usize` must land on that variant's own stem.
        for (i, icon) in ALL_ICONS.iter().enumerate() {
            assert_eq!(*icon as usize, i, "ALL_ICONS is out of declaration order");
            assert_eq!(
                STEMS[i],
                icon.stem(),
                "Icon variant #{i} and the master at that index name different icons"
            );
        }

        // ...and that order must be the one build.rs rasterizes in.
        let build_rs = include_str!("../build.rs");
        let list = build_rs
            .split_once("const ICON_STEMS: &[&str] = &[")
            .expect("build.rs still declares ICON_STEMS")
            .1
            .split_once("];")
            .expect("ICON_STEMS is terminated")
            .0;
        let from_build: Vec<&str> = list
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.trim_matches('"'))
            .collect();
        assert!(
            !from_build.is_empty(),
            "parsed nothing out of build.rs — the format changed and this guard went vacuous"
        );
        assert_eq!(
            from_build, STEMS,
            "build.rs rasterizes the icons in a different order than icons.rs embeds them"
        );
    }

    #[test]
    fn uvs_tile_the_strip_without_gaps() {
        let (a0, a1) = Icon::Left.uv();
        assert_eq!(a0[0], 0.0);
        let (b0, _) = Icon::Right.uv();
        assert_eq!(a1[0], b0[0]); // adjacent cells share an edge
        let (_, z1) = Icon::Octagon.uv();
        assert_eq!(z1[0], 1.0); // last cell ends exactly at the strip's right edge
    }
}
