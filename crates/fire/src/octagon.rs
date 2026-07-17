//! The octagon overlay — Unity VFX Graph's "octagon" particle shape, as a visualization aid.
//!
//! Unity crops a particle quad into an octagon to cut transparency overdraw. The shape has eight
//! vertices in two sets: four pinned at the quad's **edge midpoints**, and four **corner**
//! vertices that slide linearly from the quad's corners (crop 0 — the full quad, the midpoint
//! vertices degenerate onto its edges) diagonally toward the center, landing exactly on the line
//! between adjacent midpoints at crop 0.5 (the diamond). Every side connects a corner vertex to a
//! midpoint vertex, so on a square quad all eight sides stay the *same length at every crop
//! factor* — the property that makes the shape read as a regular octagon family rather than a
//! corner-cut rectangle. This module holds the pure state + geometry for drawing that shape over
//! the displayed image (or the current flipbook frame), so artists can see what an octagon-cropped
//! particle would keep of their texture.
//!
//! Split like the rest of the app: this file is pure data/math (unit-tested, no Win32/ImGui), the
//! line render is an ImGui draw list in [`crate::ui`], and the "hide outside" fade is two constant-
//! buffer floats in the pixel shader ([`crate::render::gpu`]) — never per-pixel CPU work.

use serde::{Deserialize, Serialize};

/// Crop factor bounds: 0 = the full quad, 0.5 = the diamond (the sliding vertices have met).
pub const CROP_MIN: f32 = 0.0;
pub const CROP_MAX: f32 = 0.5;
/// Default crop factor for a fresh overlay (a visibly octagonal shape).
pub const CROP_DEFAULT: f32 = 0.25;

/// The overlay's line color — a fixed palette, not a picker, so the choice stays one click.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum LineColor {
    #[default]
    Red,
    Green,
    Blue,
    White,
    Black,
}

/// Every palette entry, in the order the options window shows them.
pub const LINE_COLORS: [LineColor; 5] = [
    LineColor::Red,
    LineColor::Green,
    LineColor::Blue,
    LineColor::White,
    LineColor::Black,
];

impl LineColor {
    /// The drawn color, as sRGB RGBA (the UI pass writes through the UNORM view, so these are
    /// used as-is).
    pub fn rgba(self) -> [f32; 4] {
        match self {
            LineColor::Red => [1.0, 0.16, 0.16, 1.0],
            LineColor::Green => [0.18, 0.85, 0.25, 1.0],
            LineColor::Blue => [0.25, 0.55, 1.0, 1.0],
            LineColor::White => [1.0, 1.0, 1.0, 1.0],
            LineColor::Black => [0.0, 0.0, 0.0, 1.0],
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            LineColor::Red => "Red",
            LineColor::Green => "Green",
            LineColor::Blue => "Blue",
            LineColor::White => "White",
            LineColor::Black => "Black",
        }
    }
}

/// The whole of the overlay's state. Session-global (like the outline toggle, not per-path);
/// owned by the render surface, edited by the options window, and optionally persisted via
/// `[octagon]` in `config.toml`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OctagonState {
    /// The toolbar toggle. Always starts off; never persisted.
    pub enabled: bool,
    pub color: LineColor,
    /// The line's opacity: 0 = invisible (the shape acts only through `hide`), 1 = solid.
    pub line_opacity: f32,
    /// See [`CROP_MIN`]/[`CROP_MAX`].
    pub crop: f32,
    /// How much the image outside the octagon fades toward the viewport backdrop: 0 = fully
    /// visible, 1 = fully hidden.
    pub hide: f32,
}

impl Default for OctagonState {
    fn default() -> Self {
        OctagonState {
            enabled: false,
            color: LineColor::default(),
            line_opacity: 1.0,
            crop: CROP_DEFAULT,
            hide: 0.0,
        }
    }
}

impl OctagonState {
    /// Clamp the numeric fields into range (the sliders can't leave it, but a persisted config or
    /// a NaN from anywhere must not reach the shader).
    pub fn clamp(&mut self) {
        self.crop = if self.crop.is_finite() {
            self.crop.clamp(CROP_MIN, CROP_MAX)
        } else {
            CROP_DEFAULT
        };
        self.hide = if self.hide.is_finite() {
            self.hide.clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.line_opacity = if self.line_opacity.is_finite() {
            self.line_opacity.clamp(0.0, 1.0)
        } else {
            1.0
        };
    }

    /// The line color with [`Self::line_opacity`] applied — what the overlay actually draws with.
    pub fn line_rgba(&self) -> [f32; 4] {
        let mut c = self.color.rgba();
        c[3] *= self.line_opacity;
        c
    }
}

/// The octagon's 8 vertices for the frame rect `(x, y, w, h)` at `crop`, in the rect's own
/// coordinate space, ordered clockwise from the top edge's midpoint.
///
/// The geometry is Unity's: with half-extents normalized to 0.5, the four *midpoint* vertices are
/// pinned at `(±0.5, 0)` / `(0, ±0.5)` and the four *corner* vertices sit at `(±a, ±a)` with
/// `a = 0.5·(1 − crop)` — sliding linearly from the quad's corners at crop 0 (where the midpoint
/// vertices lie flat on its edges) to the midpoint-to-midpoint diagonal at crop 0.5 (the diamond).
/// Every side joins a corner vertex to a midpoint vertex, so on a square rect all eight sides are
/// congruent at any crop. A non-square rect stretches the shape with it, exactly as Unity's quad
/// space does.
pub fn vertices(rect: (f32, f32, f32, f32), crop: f32) -> [[f32; 2]; 8] {
    let (x, y, w, h) = rect;
    let (cx, cy) = (x + w * 0.5, y + h * 0.5);
    let a = 0.5 * (1.0 - crop.clamp(CROP_MIN, CROP_MAX));
    // Normalized offsets from the center, clockwise starting at the top midpoint.
    let n: [[f32; 2]; 8] = [
        [0.0, -0.5],
        [a, -a],
        [0.5, 0.0],
        [a, a],
        [0.0, 0.5],
        [-a, a],
        [-0.5, 0.0],
        [-a, -a],
    ];
    n.map(|[nx, ny]| [cx + nx * w, cy + ny * h])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crop_zero_is_the_quad() {
        let v = vertices((0.0, 0.0, 100.0, 100.0), 0.0);
        // Corner vertices at the rect's corners, midpoint vertices flat on its edges.
        assert_eq!(v[0], [50.0, 0.0]); // top mid
        assert_eq!(v[1], [100.0, 0.0]); // top-right corner
        assert_eq!(v[2], [100.0, 50.0]); // right mid
        assert_eq!(v[3], [100.0, 100.0]); // bottom-right corner
        assert_eq!(v[6], [0.0, 50.0]); // left mid
        assert_eq!(v[7], [0.0, 0.0]); // top-left corner
    }

    #[test]
    fn crop_half_is_the_diamond() {
        let v = vertices((0.0, 0.0, 100.0, 100.0), 0.5);
        // The midpoint vertices are the diamond's tips…
        assert_eq!(v[0], [50.0, 0.0]);
        assert_eq!(v[2], [100.0, 50.0]);
        // …and each corner vertex lands exactly on the line between adjacent tips ("in the same
        // line again"): the top-right corner at (75, 25), the midpoint of tip→tip.
        assert_eq!(v[1], [75.0, 25.0]);
        assert_eq!(v[3], [75.0, 75.0]);
    }

    #[test]
    fn interpolation_is_linear_and_follows_the_rect() {
        // Non-square rect at half-way crop 0.25: a = 0.5·(1−0.25) = 0.375 — the corner vertex is
        // half-way along its corner→diamond track, mids never move.
        let v = vertices((10.0, 20.0, 200.0, 100.0), 0.25);
        assert_eq!(v[0], [110.0, 20.0]); // top mid, pinned
        assert_eq!(v[2], [210.0, 70.0]); // right mid, pinned
        assert_eq!(v[1], [110.0 + 200.0 * 0.375, 70.0 - 100.0 * 0.375]); // top-right corner
        assert_eq!(v[5], [110.0 - 200.0 * 0.375, 70.0 + 100.0 * 0.375]); // bottom-left corner
    }

    #[test]
    fn all_sides_stay_the_same_length_on_a_square() {
        // The property the shape is chosen for: every side joins a corner vertex to a midpoint
        // vertex, so on a square rect the octagon is equilateral at *any* crop factor.
        for crop in [0.0, 0.1, 0.25, 0.4, 0.5] {
            let v = vertices((0.0, 0.0, 100.0, 100.0), crop);
            let len = |a: [f32; 2], b: [f32; 2]| ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt();
            let first = len(v[0], v[1]);
            for i in 0..8 {
                let side = len(v[i], v[(i + 1) % 8]);
                assert!((side - first).abs() < 1e-3, "crop {crop}, side {i}: {side} vs {first}");
            }
        }
    }

    #[test]
    fn state_clamps_out_of_range_and_nan() {
        let mut s = OctagonState {
            enabled: true,
            color: LineColor::Blue,
            line_opacity: 7.0,
            crop: 3.0,
            hide: -1.0,
        };
        s.clamp();
        assert_eq!(s.crop, CROP_MAX);
        assert_eq!(s.hide, 0.0);
        assert_eq!(s.line_opacity, 1.0);
        s.crop = f32::NAN;
        s.hide = f32::INFINITY;
        s.line_opacity = f32::NAN;
        s.clamp();
        assert_eq!(s.crop, CROP_DEFAULT);
        assert_eq!(s.hide, 0.0);
        assert_eq!(s.line_opacity, 1.0);
    }
}
