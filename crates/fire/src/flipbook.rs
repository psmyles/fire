//! Flipbook (sprite-sheet) viewer mode — pure logic, no Win32/D3D.
//!
//! A flipbook texture is a single still image laid out as a `cols × rows` grid of animation
//! frames (row-major, left→right, top→bottom). This module holds everything that can be
//! reasoned about and unit-tested without a window or GPU:
//!
//!   * the per-image [`FlipbookState`] the UI mutates and the [`PerPath`] map value the shell
//!     keys by path,
//!   * the frame math the renderer needs ([`resolve_frames`], [`frame_cell_offset`],
//!     [`frame_dims`], [`max_lod`]), and
//!   * grid auto-detection ([`detect`]) run off-thread on the decode worker — **the pixels decide
//!     the grid** (YIN period detection over the luma, then alpha, channel; §Detection), because
//!     filenames can be wrong or missing. A `_8x8` filename token is only a last-resort fallback
//!     for a sheet content can't resolve, never an override. The result is surfaced solely as a
//!     dismissible hint (it never enters the mode on its own).
//!
//! [`crate::render::gpu`] turns the active state into constant-buffer values; [`crate::win`]
//! owns the `HashMap<PathBuf, PerPath>` and drives playback via a timer.

use std::path::Path;

use fire_decode::{DecodedImage, PixelFormat};

/// Playback rate bounds and default (frames per second).
pub const FPS_DEFAULT: f32 = 24.0;
pub const FPS_MIN: f32 = 0.1;
pub const FPS_MAX: f32 = 120.0;

/// Inclusive bounds on a grid axis. `1` is allowed on a single axis (strip sheets) but a `1×1`
/// "grid" is meaningless and always rejected.
pub const GRID_MIN: u32 = 1;
pub const GRID_MAX: u32 = 64;

/// A detected or user-entered grid: `cols` across, `rows` down. Frame `i` lives at
/// `(i % cols, i / cols)` (row-major).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grid {
    pub cols: u32,
    pub rows: u32,
}

impl Grid {
    pub fn new(cols: u32, rows: u32) -> Self {
        Self { cols, rows }
    }

    /// Total cells in the grid (the default and maximum frame count).
    pub fn cells(self) -> u32 {
        self.cols.saturating_mul(self.rows)
    }
}

/// Per-image flipbook settings. The UI widgets write these; the shell's timer advances
/// `frame_pos` during playback. Retained (via [`PerPath`]) across navigation and disk reloads
/// so a texture keeps its own state for the session.
#[derive(Debug, Clone)]
pub struct FlipbookState {
    pub grid: Grid,
    /// Frames actually played, `1..=grid.cells()`. Defaults to the full grid; a smaller value
    /// covers sheets whose last row is partially empty.
    pub frame_count: u32,
    pub fps: f32,
    /// Crossfade consecutive frames. When on, `frame_pos` is used fractionally (both during
    /// playback and slider scrubbing); when off, only the integer part is shown.
    pub blend: bool,
    pub playing: bool,
    /// Fractional playback position in `[0, frame_count)`; the UI scrubs it, the app advances it.
    pub frame_pos: f32,
}

impl FlipbookState {
    /// A fresh state adopting `grid`: full frame count, 24 fps, no blend, playing from frame 0.
    pub fn new(grid: Grid) -> Self {
        let grid = Grid::new(
            grid.cols.clamp(GRID_MIN, GRID_MAX),
            grid.rows.clamp(GRID_MIN, GRID_MAX),
        );
        Self {
            grid,
            frame_count: grid.cells().max(1),
            fps: FPS_DEFAULT,
            blend: false,
            playing: true,
            frame_pos: 0.0,
        }
    }

    /// Re-establish every invariant after an edit: grid axes in range, `frame_count` in
    /// `1..=cells`, `fps` in range, `frame_pos` wrapped into `[0, frame_count)`.
    pub fn clamp(&mut self) {
        self.grid.cols = self.grid.cols.clamp(GRID_MIN, GRID_MAX);
        self.grid.rows = self.grid.rows.clamp(GRID_MIN, GRID_MAX);
        let cells = self.grid.cells().max(1);
        self.frame_count = self.frame_count.clamp(1, cells);
        self.fps = self.fps.clamp(FPS_MIN, FPS_MAX);
        let count = self.frame_count as f32;
        if !self.frame_pos.is_finite() {
            self.frame_pos = 0.0;
        } else {
            self.frame_pos = self.frame_pos.rem_euclid(count);
        }
    }
}

/// The map value the shell stores per image path (session-only). `state` survives a disable so
/// re-enabling restores the user's settings; `hint`/`hint_dismissed` drive the hint chip.
#[derive(Debug, Clone, Default)]
pub struct PerPath {
    pub enabled: bool,
    pub state: Option<FlipbookState>,
    pub hint: Option<Grid>,
    pub hint_dismissed: bool,
}

// ---------------------------------------------------------------------------------------------
// Frame math (consumed by the renderer and the transport bar).
// ---------------------------------------------------------------------------------------------

/// Resolve a fractional playback position into the two frames to sample and the blend factor.
///
/// Returns `(frame_a, frame_b, blend)` where `frame_b == (frame_a + 1) % frame_count` — so the
/// loop seam blends the last frame back to frame 0. With `blend` off (or a degenerate count) the
/// result is a hard cut: `blend == 0` and `frame_b == frame_a`. `frame_count <= 1` yields
/// `(0, 0, 0)`.
pub fn resolve_frames(frame_pos: f32, frame_count: u32, blend: bool) -> (u32, u32, f32) {
    if frame_count <= 1 {
        return (0, 0, 0.0);
    }
    let count = frame_count;
    let p = if frame_pos.is_finite() {
        frame_pos.rem_euclid(count as f32)
    } else {
        0.0
    };
    let a = (p.floor() as u32).min(count - 1);
    if !blend {
        return (a, a, 0.0);
    }
    let t = (p - p.floor()).clamp(0.0, 1.0);
    let b = (a + 1) % count;
    (a, b, t)
}

/// UV/texel origin of `frame`'s cell within the sheet (row-major). Cell size is fractional so
/// non-divisible sheet dimensions place cells exactly (the renderer works in texels).
pub fn frame_cell_offset(frame: u32, grid: Grid, sheet: (u32, u32)) -> (f32, f32) {
    let cols = grid.cols.max(1);
    let rows = grid.rows.max(1);
    let col = frame % cols;
    let row = (frame / cols).min(rows - 1);
    let cell_w = sheet.0 as f32 / cols as f32;
    let cell_h = sheet.1 as f32 / rows as f32;
    (col as f32 * cell_w, row as f32 * cell_h)
}

/// Integer frame dimensions for the CPU pan/zoom/fit math (rounded; sub-pixel error is
/// invisible in fit/clamp). The shader uses exact fractional cell sizes.
pub fn frame_dims(grid: Grid, sheet: (u32, u32)) -> (u32, u32) {
    let cols = grid.cols.max(1);
    let rows = grid.rows.max(1);
    let w = (sheet.0 as f32 / cols as f32).round() as u32;
    let h = (sheet.1 as f32 / rows as f32).round() as u32;
    (w.max(1), h.max(1))
}

/// Mip-LOD clamp for flipbook sampling. The sheet carries a full mip chain; high mips average
/// across cell boundaries and would ghost neighbouring frames into a minified frame. Clamp to
/// the coarsest mip that still stays within a cell: `log2(min_cell_px) - 1`, floored at 0.
pub fn max_lod(grid: Grid, sheet: (u32, u32)) -> f32 {
    let (w, h) = frame_dims(grid, sheet);
    let min_cell = w.min(h).max(1) as f32;
    (min_cell.log2() - 1.0).max(0.0)
}

// ---------------------------------------------------------------------------------------------
// Detection: filename prior + content analysis.
// ---------------------------------------------------------------------------------------------

/// Longest-axis cap for the grayscale analysis copy (integer box-binned down to this).
const DETECT_MAX_DIM: u32 = 512;
/// Max samples per axis averaged inside each source bin when building the analysis copy. A large
/// image bins many source pixels into each ≤512px output pixel; averaging *every* one makes the
/// scan cost scale with the full pixel count (≈0.5s for a 16K sheet). Averaging a bounded `N×N`
/// grid per bin instead caps total reads at `≈ 512²·N²`, so the scan is ≈ constant time — a 16K
/// sheet costs about the same as a 2K one, and a 2K sheet roughly halves — while `N×N` samples
/// still anti-alias each bin enough to place the cell gutters (which are far wider than a bin).
/// `2` measured a clean ~2× speedup with zero detection change across the test corpus; `1` (pure
/// point-sampling) is ~4× but risks aliasing fine textures into false periodicity.
const THUMB_SAMPLES_PER_AXIS: u32 = 2;
/// Smallest cell size, in thumbnail pixels, a period candidate may have (a smaller "cell" is
/// noise, not a frame). Also the minimum autocorrelation lag searched.
const MIN_CELL_THUMB: usize = 6;
/// Profile variance (mean-subtracted, luma-gradient units) below which an axis is "flat" — a
/// uniform texture with no gutters. Below this there is no grid to find.
const PROFILE_VAR_EPS: f32 = 1e-6;
/// YIN absolute threshold: the cumulative-mean-normalized difference must dip below this at the
/// fundamental period. Real grids sit in a tight cluster of deep dips (`d' < 0.15`) well
/// separated from textures and full-bleed non-grids (`d' > 0.55`); this sits in the empty gap
/// between them for a wide margin on both sides. Full-bleed animations whose gutters dissolve
/// (fire/smoke) fall in the upper cluster and rely on the filename token instead.
const YIN_THRESHOLD: f32 = 0.35;
/// Variance (on 0..1 luma) below which a cell is "flat" and excluded from similarity scoring.
const FLAT_VAR_EPS: f32 = 1e-4;
/// Minimum fraction of non-flat cells for the detected grid to stand — a period that splits a
/// localized sprite into mostly-empty cells is spurious (over-split guard).
const MIN_NONFLAT_FRAC: f32 = 0.5;
/// Minimum frame count along a strip's one animated axis (`N×1` / `1×N`). A 2-frame strip is too
/// weak to claim from content; a 2-D grid is allowed at 2×2 (4 cells) by the general cell floor.
const STRIP_MIN_FRAMES: u32 = 3;
/// Minimum adjacent-cell median NCC for a strip. A strip has only one axis of evidence, so it also
/// must look like an *animation* — consecutive frames resembling each other. A segmented object
/// whose bands read as a strip (bat wing → `1×3`, adjacent ≈ 0.25) fails this; a real strip
/// (drifting plant/flame frames, adjacent ≈ 0.6) clears it.
const STRIP_ADJ_MIN: f32 = 0.45;
/// Median NCC between cells a half-loop apart at/above which the sheet is a near-perfect *tiling*
/// (identical cells, not evolving animation frames) and rejected. Real animations — even smooth
/// smoke — evolve enough to stay below this; a pixel-repeating texture sits at 1.0.
const FAR_IDENTICAL_CAP: f32 = 0.999;

/// Detect the flipbook grid of a decoded sheet, or `None`. **Content is the determining factor:**
/// the pixels decide the grid, because filenames can be wrong or missing. A `NxM` filename token
/// is used *only* as a fallback when content analysis cannot resolve a grid at all — it never
/// overrides what the pixels show. The content detector finds the grid of a regular sprite sheet
/// (including non-power-of-two grids on a power-of-two atlas, and 1×N / N×1 strips) and returns
/// `None` for a lone sprite or a texture. Runs on the decode worker; a few ms of pure Rust over a
/// ≤512px grayscale copy.
pub fn detect(path: &Path, image: &DecodedImage) -> Option<Grid> {
    content_grid(image).or_else(|| filename_prior(path))
}

/// Parse a `NxM` grid token from the filename (case-insensitive `x`, digits only, no regex).
/// Mirrors the channel-suffix scan: split the stem on `_ - .` and take the first token (scanning
/// right-to-left) that parses. `cols × rows`, both in `1..=64`; `1x1` rejected. Only a prior.
pub fn filename_prior(path: &Path) -> Option<Grid> {
    let stem = path.file_stem()?.to_str()?;
    for tok in stem.rsplit(['_', '-', '.']) {
        if let Some(g) = parse_grid_token(tok) {
            return Some(g);
        }
    }
    None
}

fn parse_grid_token(tok: &str) -> Option<Grid> {
    let bytes = tok.as_bytes();
    // Find the single separating 'x'/'X'.
    let xi = bytes.iter().position(|&b| b == b'x' || b == b'X')?;
    let (a, b) = (&tok[..xi], &tok[xi + 1..]);
    if a.is_empty()
        || b.is_empty()
        || !a.bytes().all(|c| c.is_ascii_digit())
        || !b.bytes().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let cols: u32 = a.parse().ok()?;
    let rows: u32 = b.parse().ok()?;
    if !(GRID_MIN..=GRID_MAX).contains(&cols) || !(GRID_MIN..=GRID_MAX).contains(&rows) {
        return None;
    }
    if cols == 1 && rows == 1 {
        return None;
    }
    Some(Grid::new(cols, rows))
}

/// Content analysis: detect the grid from the pixels alone. `None` when the image shows no
/// sprite-sheet structure. Tries the luminance channel first (colour/brightness structure — the
/// usual case), then falls back to the alpha channel (structure carried in transparency — a mask
/// over flat RGB, or a crisper alpha edge than the RGB). Each channel independently rejects
/// non-grids, so the fallback widens what we can find without adding false positives. The alpha
/// pass is skipped when the image is fully opaque (its alpha carries nothing).
fn content_grid(image: &DecodedImage) -> Option<Grid> {
    grid_from_signal(image, Signal::Luma).or_else(|| {
        (!image.alpha_opaque)
            .then(|| grid_from_signal(image, Signal::Alpha))
            .flatten()
    })
}

/// Detect a grid from one channel of the image (see [`content_grid`] for the cascade). `None` if
/// that channel shows no regular sprite-sheet periodicity.
fn grid_from_signal(image: &DecodedImage, signal: Signal) -> Option<Grid> {
    let (thumb, tw, th) = analysis_thumbnail(image, signal)?;

    // Per-axis "content activity" profiles: `col_act[x]` is the mean vertical detail in column x,
    // `row_act[y]` the mean horizontal detail in row y. A sprite sheet lays content in a regular
    // grid of cells separated by empty gutters, so each profile is *periodic* — high over cells,
    // low over gutters — with period = one cell. A single object (one broad bump) or a uniform
    // texture (flat profile) has no such periodicity.
    let col_act = axis_activity(&thumb, tw, th, Axis::Col);
    let row_act = axis_activity(&thumb, tw, th, Axis::Row);

    // A full grid has a period on both axes; a strip (`N×1` / `1×N`) has one on the animation axis
    // and none on the other (each frame spans the sheet's short dimension). Take rows/cols = 1
    // when that axis carries no period.
    let grid = match (axis_period(&col_act, tw), axis_period(&row_act, th)) {
        (Some((c, _)), Some((r, _))) => Grid::new(c, r),
        (Some((c, _)), None) => Grid::new(c, 1), // horizontal strip
        (None, Some((r, _))) => Grid::new(1, r), // vertical strip
        (None, None) => return None,
    };
    let (min_axis, max_axis) = (grid.cols.min(grid.rows), grid.cols.max(grid.rows));
    let is_strip = min_axis == 1;
    // Enough frames to be an animation: a 2-D grid needs ≥ 4 cells (2×2); a strip needs ≥ 3 frames
    // along its one axis (a 2-frame "animation" is too weak to claim from content alone).
    if grid.cells() < 4 && !(is_strip && max_axis >= STRIP_MIN_FRAMES) {
        return None;
    }

    // Content guards on the frame sequence. YIN already required regular gutters (an irregular
    // texture yields no period), so these reject the residual shapes:
    //   * a mostly-*empty* split (a localized sprite over-split into blank cells), and
    //   * a near-*perfect tiling* — cells identical even a half-loop apart (`far ≈ 1`). A real
    //     animation always evolves, so even smooth smoke/fire sheets stay below the cap (measured
    //     `far ≤ 0.9955`) while a pixel-repeating texture sits at exactly 1.0.
    //   * for a **strip** only (one axis of evidence, so easier to fool): the frames must resemble
    //     each other (`adjacent ≥ STRIP_ADJ_MIN`) — a segmented object whose bands read as a strip
    //     (a bat wing → `1×3`, adjacent ≈ 0.25) has dissimilar "frames" and is rejected.
    let (adj, far, nonflat_frac) = cell_sequence_stats(&thumb, tw, th, grid);
    if nonflat_frac < MIN_NONFLAT_FRAC || far >= FAR_IDENTICAL_CAP {
        return None;
    }
    if is_strip && adj < STRIP_ADJ_MIN {
        return None;
    }
    Some(grid)
}

/// Detect the fundamental grid period along one axis from its activity `profile` (the thumbnail is
/// `thumb_dim` samples along this axis). Returns `(cells, confidence)` where `cells =
/// round(thumb_dim / period)` in `2..=GRID_MAX`, or `None` when the profile shows no clear
/// repeating period (a single object or a flat texture).
///
/// Method: the YIN cumulative-mean-normalized difference function. `d(τ) = mean_i (a[i] −
/// a[i+τ])²` dips toward 0 at the true period (the profile lines up with itself one cell over);
/// normalizing by the running mean of `d` removes the "small lag ⇒ small difference" bias and,
/// crucially, the amplitude *envelope* bias — a flipbook whose frames grow/fade across the sheet
/// still has aligned gutters, so the dip survives even though a raw autocorrelation would lock
/// onto the low-frequency envelope instead (that bug detected FireFar's 8×8 as 5×5). The
/// fundamental is the smallest-lag dip below [`YIN_THRESHOLD`]; confidence is the dip depth.
fn axis_period(profile: &[f32], thumb_dim: u32) -> Option<(u32, f32)> {
    let l = profile.len();
    if l < 16 {
        return None;
    }
    let mean = profile.iter().sum::<f32>() / l as f32;
    let var: f32 = profile.iter().map(|v| (v - mean).powi(2)).sum::<f32>() / l as f32;
    if var <= PROFILE_VAR_EPS {
        return None; // flat profile: uniform texture, no gutters
    }

    // Search lags for 2..=GRID_MAX cells; need ≥2 periods of support, so τ ≤ l/2.
    let tau_min = (l / GRID_MAX as usize).max(MIN_CELL_THUMB);
    let tau_max = l / 2;
    if tau_min >= tau_max {
        return None;
    }

    // YIN difference function d(τ) = mean squared difference of the profile against itself at lag
    // τ, then the cumulative-mean normalization d'(τ) = d(τ)·τ / Σ_{j≤τ} d(j).
    let mut d = vec![0.0f64; tau_max + 1];
    for (tau, dv) in d.iter_mut().enumerate().skip(1) {
        let n = l - tau;
        let mut s = 0.0f64;
        for i in 0..n {
            let diff = (profile[i] - profile[i + tau]) as f64;
            s += diff * diff;
        }
        *dv = s / n as f64;
    }
    let mut dp = vec![1.0f64; tau_max + 1];
    let mut running = 0.0f64;
    for tau in 1..=tau_max {
        running += d[tau];
        dp[tau] = if running > 0.0 {
            d[tau] * tau as f64 / running
        } else {
            1.0
        };
    }

    // Smallest-lag local minimum of d' that drops below the threshold (YIN "absolute threshold").
    let mut best: Option<(u32, f32)> = None;
    for tau in tau_min..tau_max {
        if dp[tau] < YIN_THRESHOLD as f64 && dp[tau] <= dp[tau - 1] && dp[tau] <= dp[tau + 1] {
            let cells = ((thumb_dim as f32 / tau as f32).round() as u32).clamp(2, GRID_MAX);
            let conf = (1.0 - dp[tau]).clamp(0.0, 1.0) as f32;
            best = Some((cells, conf));
            break;
        }
    }
    best
}

enum Axis {
    Col,
    Row,
}

/// Per-axis gradient-activity profile: for columns, `act[x] = mean_y |L[x+1,y] − L[x,y]|`
/// (length `dim-1`); rows analogously.
fn axis_activity(thumb: &[f32], tw: u32, th: u32, axis: Axis) -> Vec<f32> {
    let (tw, th) = (tw as usize, th as usize);
    match axis {
        Axis::Col => {
            let mut act = vec![0.0f32; tw.saturating_sub(1)];
            for (x, a) in act.iter_mut().enumerate() {
                let mut s = 0.0;
                for y in 0..th {
                    s += (thumb[y * tw + x + 1] - thumb[y * tw + x]).abs();
                }
                *a = s / th as f32;
            }
            act
        }
        Axis::Row => {
            let mut act = vec![0.0f32; th.saturating_sub(1)];
            for (y, a) in act.iter_mut().enumerate() {
                let mut s = 0.0;
                for x in 0..tw {
                    s += (thumb[(y + 1) * tw + x] - thumb[y * tw + x]).abs();
                }
                *a = s / tw as f32;
            }
            act
        }
    }
}

/// Summarize the detected grid's cells for the content guards. Returns `(adjacent, far,
/// nonflat_frac)` — median NCC (on 8×8 mean-subtracted unit cell descriptors) between consecutive
/// cells and between cells a half-loop apart, and the fraction of non-flat cells:
///   * `adjacent` low means neighbouring cells don't resemble each other — a *segmented object*
///     (e.g. a bat wing whose bands read as a strip), not an animation. Used to guard strips,
///     which have only one axis of evidence.
///   * `far ≈ 1` means every cell is identical even half a loop away — a repeating *texture*, not
///     evolving frames (a real animation, even smooth smoke, drifts below 1).
///   * low `nonflat_frac` means the grid over-split a localized sprite into blank cells.
fn cell_sequence_stats(thumb: &[f32], tw: u32, th: u32, grid: Grid) -> (f32, f32, f32) {
    let cols = grid.cols.max(1);
    let rows = grid.rows.max(1);
    let n = (cols * rows) as usize;
    if n < 2 {
        return (0.0, 0.0, 0.0);
    }
    let mut descs: Vec<Option<[f32; 64]>> = Vec::with_capacity(n);
    for r in 0..rows {
        let y0 = (r as f32 * th as f32 / rows as f32).round() as u32;
        let y1 = ((r + 1) as f32 * th as f32 / rows as f32).round() as u32;
        for c in 0..cols {
            let x0 = (c as f32 * tw as f32 / cols as f32).round() as u32;
            let x1 = ((c + 1) as f32 * tw as f32 / cols as f32).round() as u32;
            descs.push(cell_descriptor(thumb, tw, x0, x1, y0, y1));
        }
    }
    let nonflat = descs.iter().filter(|d| d.is_some()).count();
    let nonflat_frac = nonflat as f32 / n as f32;
    let ncc = |i: usize, j: usize| -> Option<f32> {
        match (&descs[i], &descs[j]) {
            (Some(a), Some(b)) => Some(a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()),
            _ => None,
        }
    };
    let adj: Vec<f32> = (0..n - 1).filter_map(|i| ncc(i, i + 1)).collect();
    let half = (n / 2).max(1);
    let far: Vec<f32> = (0..n - half).filter_map(|i| ncc(i, i + half)).collect();
    (median(&adj), median(&far), nonflat_frac)
}

/// Box-downsample a cell rect to an 8×8, mean-subtracted, L2-normalized descriptor. `None` if
/// the cell's variance is below [`FLAT_VAR_EPS`] (a near-flat cell carries no signal).
fn cell_descriptor(
    thumb: &[f32],
    tw: u32,
    x0: u32,
    x1: u32,
    y0: u32,
    y1: u32,
) -> Option<[f32; 64]> {
    let (x1, y1) = (x1.max(x0 + 1), y1.max(y0 + 1));
    let cw = x1 - x0;
    let ch = y1 - y0;
    let tw = tw as usize;
    let mut d = [0.0f32; 64];
    for (by, dcell) in d.chunks_exact_mut(8).enumerate() {
        let sy0 = y0 + (by as u32 * ch) / 8;
        let sy1 = (y0 + ((by as u32 + 1) * ch) / 8).max(sy0 + 1);
        for (bx, out) in dcell.iter_mut().enumerate() {
            let sx0 = x0 + (bx as u32 * cw) / 8;
            let sx1 = (x0 + ((bx as u32 + 1) * cw) / 8).max(sx0 + 1);
            let mut s = 0.0f32;
            let mut cnt = 0u32;
            for y in sy0..sy1 {
                for x in sx0..sx1 {
                    s += thumb[y as usize * tw + x as usize];
                    cnt += 1;
                }
            }
            *out = if cnt > 0 { s / cnt as f32 } else { 0.0 };
        }
    }
    // Mean-subtract, then L2-normalize.
    let mean: f32 = d.iter().sum::<f32>() / 64.0;
    for v in d.iter_mut() {
        *v -= mean;
    }
    let norm: f32 = d.iter().map(|v| v * v).sum::<f32>().sqrt();
    // norm² == 64·variance; flat guard is on variance.
    if norm * norm / 64.0 < FLAT_VAR_EPS {
        return None;
    }
    for v in d.iter_mut() {
        *v /= norm;
    }
    Some(d)
}

/// Build the ≤512px grayscale analysis copy of `signal` (luma or alpha, 0..1-ish) via integer box
/// binning. Returns `(pixels, thumb_w, thumb_h)`; `None` if the image is too small to analyze.
fn analysis_thumbnail(image: &DecodedImage, signal: Signal) -> Option<(Vec<f32>, u32, u32)> {
    let (w, h) = (image.width, image.height);
    if w < 2 || h < 2 {
        return None;
    }
    let b = w.max(h).div_ceil(DETECT_MAX_DIM).max(1);
    let tw = (w / b).max(1);
    let th = (h / b).max(1);
    let bpp = image.format.bytes_per_pixel();
    let stride = w as usize * bpp;
    let px = &image.pixels;
    // Step within each bin so at most `THUMB_SAMPLES_PER_AXIS` samples are read per axis (bounding
    // total reads to ≈ `tw·th·N²`); `1` for a small bin means "read every pixel" (no sub-sampling).
    let step = (b / THUMB_SAMPLES_PER_AXIS).max(1);

    let mut out = vec![0.0f32; (tw * th) as usize];
    for oy in 0..th {
        for ox in 0..tw {
            let mut s = 0.0f32;
            let mut cnt = 0u32;
            let mut yy = 0;
            while yy < b {
                let sy = oy * b + yy;
                if sy >= h {
                    break;
                }
                let mut xx = 0;
                while xx < b {
                    let sx = ox * b + xx;
                    if sx >= w {
                        break;
                    }
                    let off = sy as usize * stride + sx as usize * bpp;
                    s += sample_at(px, off, image.format, signal);
                    cnt += 1;
                    xx += step;
                }
                yy += step;
            }
            out[(oy * tw + ox) as usize] = if cnt > 0 { s / cnt as f32 } else { 0.0 };
        }
    }
    Some((out, tw, th))
}

/// Which channel the analysis thumbnail samples. A sprite sheet's cell/gutter structure can live
/// in either: colour/brightness ([`Signal::Luma`], the usual case) or transparency
/// ([`Signal::Alpha`] — VFX exports whose frames are an opacity mask over flat RGB, e.g. steam on
/// solid white, or fire whose alpha edge is crisper than its smoky RGB). Detection tries luma
/// first and falls back to alpha.
#[derive(Clone, Copy)]
enum Signal {
    Luma,
    Alpha,
}

/// The analysis scalar (0..1-ish; HDR luma may exceed 1, which is fine — scores are relative) of
/// the RGBA pixel at byte offset `off`, per format and [`Signal`]. Mirrors the per-format lane
/// handling in `fire_decode`'s `alpha_is_opaque`.
fn sample_at(px: &[u8], off: usize, format: PixelFormat, signal: Signal) -> f32 {
    let (r, g, b, a) = match format {
        PixelFormat::Rgba8Unorm => (
            px[off] as f32 / 255.0,
            px[off + 1] as f32 / 255.0,
            px[off + 2] as f32 / 255.0,
            px[off + 3] as f32 / 255.0,
        ),
        PixelFormat::Rgba16Unorm => (
            u16::from_ne_bytes([px[off], px[off + 1]]) as f32 / 65535.0,
            u16::from_ne_bytes([px[off + 2], px[off + 3]]) as f32 / 65535.0,
            u16::from_ne_bytes([px[off + 4], px[off + 5]]) as f32 / 65535.0,
            u16::from_ne_bytes([px[off + 6], px[off + 7]]) as f32 / 65535.0,
        ),
        PixelFormat::Rgba16Float => (
            half_to_f32(u16::from_ne_bytes([px[off], px[off + 1]])),
            half_to_f32(u16::from_ne_bytes([px[off + 2], px[off + 3]])),
            half_to_f32(u16::from_ne_bytes([px[off + 4], px[off + 5]])),
            half_to_f32(u16::from_ne_bytes([px[off + 6], px[off + 7]])),
        ),
        PixelFormat::Rgba32Float => (
            f32::from_ne_bytes([px[off], px[off + 1], px[off + 2], px[off + 3]]),
            f32::from_ne_bytes([px[off + 4], px[off + 5], px[off + 6], px[off + 7]]),
            f32::from_ne_bytes([px[off + 8], px[off + 9], px[off + 10], px[off + 11]]),
            f32::from_ne_bytes([px[off + 12], px[off + 13], px[off + 14], px[off + 15]]),
        ),
    };
    match signal {
        Signal::Luma => 0.2126 * r + 0.7152 * g + 0.0722 * b,
        Signal::Alpha => a,
    }
}

/// Minimal IEEE half → f32 (no dependency). Handles normals, subnormals, zero, inf/NaN.
fn half_to_f32(h: u16) -> f32 {
    let sign = (h >> 15) & 1;
    let exp = (h >> 10) & 0x1f;
    let mant = h & 0x3ff;
    let val = match exp {
        0 => (mant as f32) * 2f32.powi(-24), // subnormal (and zero when mant==0)
        0x1f => {
            if mant == 0 {
                f32::INFINITY
            } else {
                f32::NAN
            }
        }
        _ => (1.0 + mant as f32 / 1024.0) * 2f32.powi(exp as i32 - 15),
    };
    if sign == 1 {
        -val
    } else {
        val
    }
}

/// Median of a slice (mean of the two middle elements for even length). `0` for empty input.
fn median(v: &[f32]) -> f32 {
    if v.is_empty() {
        return 0.0;
    }
    let mut s: Vec<f32> = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = s.len();
    if n % 2 == 1 {
        s[n / 2]
    } else {
        0.5 * (s[n / 2 - 1] + s[n / 2])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // ---- filename prior --------------------------------------------------------------------

    fn prior(name: &str) -> Option<Grid> {
        filename_prior(&PathBuf::from(name))
    }

    #[test]
    fn filename_accept_and_reject() {
        assert_eq!(prior("run_8x8.png"), Some(Grid::new(8, 8)));
        assert_eq!(prior("explosion_4x4_v2.tga"), Some(Grid::new(4, 4))); // right-to-left skips v2
        assert_eq!(prior("sheet-2X8.png"), Some(Grid::new(2, 8))); // case-insensitive x
        assert_eq!(prior("T_x_8x8.png"), Some(Grid::new(8, 8)));
        assert_eq!(prior("smoke_6x3.exr"), Some(Grid::new(6, 3)));
        assert_eq!(prior("a_1x1.png"), None); // 1x1 rejected
        assert_eq!(prior("a_65x2.png"), None); // out of range
        assert_eq!(prior("photo.png"), None); // no token
        assert_eq!(prior("render_1920x1080.png"), None); // resolution, out of range
    }

    // ---- frame math ------------------------------------------------------------------------

    #[test]
    fn resolve_frames_blend_off_snaps() {
        // Blend off: hard cut, b == a, t == 0.
        assert_eq!(resolve_frames(3.7, 16, false), (3, 3, 0.0));
        assert_eq!(resolve_frames(0.0, 16, false), (0, 0, 0.0));
    }

    #[test]
    fn resolve_frames_blend_on_and_seam() {
        let (a, b, t) = resolve_frames(3.25, 16, true);
        assert_eq!((a, b), (3, 4));
        assert!((t - 0.25).abs() < 1e-6);
        // Loop seam: last frame blends back to frame 0.
        let (a, b, t) = resolve_frames(15.5, 16, true);
        assert_eq!((a, b), (15, 0));
        assert!((t - 0.5).abs() < 1e-6);
    }

    #[test]
    fn resolve_frames_count_one_and_negative_wrap() {
        assert_eq!(resolve_frames(0.5, 1, true), (0, 0, 0.0));
        // Negative frame_pos wraps safely.
        let (a, b, _) = resolve_frames(-0.5, 16, true);
        assert_eq!((a, b), (15, 0));
    }

    #[test]
    fn cell_offset_row_major_and_fractional() {
        // 3×3 over a non-divisible 1000×1000 sheet → fractional cells.
        let (x, y) = frame_cell_offset(4, Grid::new(3, 3), (1000, 1000)); // frame 4 = col 1, row 1
        assert!((x - 1000.0 / 3.0).abs() < 1e-3);
        assert!((y - 1000.0 / 3.0).abs() < 1e-3);
        // Row-major: frame 2 in a 2-col grid is col 0, row 1.
        let (x, y) = frame_cell_offset(2, Grid::new(2, 4), (128, 256));
        assert_eq!((x, y), (0.0, 64.0));
    }

    #[test]
    fn frame_dims_and_max_lod() {
        assert_eq!(frame_dims(Grid::new(8, 8), (128, 128)), (16, 16));
        // 16px cell: log2(16) - 1 = 3.
        assert!((max_lod(Grid::new(8, 8), (128, 128)) - 3.0).abs() < 1e-6);
    }

    #[test]
    fn state_clamp_invariants() {
        let mut s = FlipbookState::new(Grid::new(8, 8));
        s.grid = Grid::new(100, 0);
        s.frame_count = 9999;
        s.fps = 1000.0;
        s.frame_pos = 200.0;
        s.clamp();
        assert_eq!(s.grid, Grid::new(64, 1));
        assert_eq!(s.frame_count, 64);
        assert_eq!(s.fps, FPS_MAX);
        assert!(s.frame_pos >= 0.0 && s.frame_pos < 64.0);
    }

    // ---- detection: synthetic sheets -------------------------------------------------------

    /// Build an Rgba8 `DecodedImage` from a luma closure `f(x, y) -> 0..=255` (gray, opaque).
    fn img_from<F: Fn(u32, u32) -> u8>(w: u32, h: u32, f: F) -> DecodedImage {
        let mut px = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let v = f(x, y);
                px.extend_from_slice(&[v, v, v, 255]);
            }
        }
        DecodedImage {
            pixels: px,
            width: w,
            height: h,
            format: PixelFormat::Rgba8Unorm,
            bit_depth: 8,
            channels: 4,
            alpha_opaque: true,
            icc: None,
            source_format: "TEST",
            downscaled_from: None,
            animation: None,
        }
    }

    /// A localized bright blob that drifts a little each frame: consecutive cells overlap (high
    /// similarity), but the blob occupies only part of a cell — so a finer over-split (16×16 on
    /// an 8×8 sheet) leaves mostly-empty cells and is rejected by the non-flat-fraction guard.
    fn moving_dot_sheet(cols: u32, rows: u32, cell: u32) -> DecodedImage {
        let (w, h) = (cols * cell, rows * cell);
        img_from(w, h, |x, y| {
            let c = x / cell;
            let r = y / cell;
            let frame = (r * cols + c) as f32;
            let lx = (x % cell) as f32;
            let ly = (y % cell) as f32;
            // Blob oscillates within the cell: enough per-frame motion that consecutive cells
            // are similar-but-distinct (NCC in-band), bounded so the blob stays localized.
            let cx = cell as f32 * (0.5 + 0.2 * (frame * 0.9).sin());
            let cy = cell as f32 * (0.5 + 0.2 * (frame * 1.3).cos());
            let d2 = (lx - cx).powi(2) + (ly - cy).powi(2);
            let sig = cell as f32 * 0.15; // small → localized
            let v = (-d2 / (2.0 * sig * sig)).exp();
            (v * 255.0) as u8
        })
    }

    /// A `cols×rows` grid of drifting blobs over an arbitrary `w×h` canvas — cell size `w/cols`,
    /// `h/rows` may be fractional (a 5×5 on 512² has 102.4px cells). Exercises the period detector
    /// on non-power-of-two grids, the common real-world layout the old divisor scan couldn't see.
    fn moving_dot_sheet_frac(cols: u32, rows: u32, w: u32, h: u32) -> DecodedImage {
        let cw = w as f32 / cols as f32;
        let ch = h as f32 / rows as f32;
        img_from(w, h, |x, y| {
            let c = ((x as f32 / cw).floor() as u32).min(cols - 1);
            let r = ((y as f32 / ch).floor() as u32).min(rows - 1);
            let frame = (r * cols + c) as f32;
            let lx = x as f32 - c as f32 * cw;
            let ly = y as f32 - r as f32 * ch;
            let bx = cw * (0.5 + 0.2 * (frame * 0.9).sin());
            let by = ch * (0.5 + 0.2 * (frame * 1.3).cos());
            let d2 = (lx - bx).powi(2) + (ly - by).powi(2);
            let sig = cw.min(ch) * 0.15;
            let v = (-d2 / (2.0 * sig * sig)).exp();
            (v * 255.0) as u8
        })
    }

    #[test]
    fn detect_moving_dot_no_prior() {
        let img = moving_dot_sheet(8, 8, 16); // 128×128, full-bleed
        assert_eq!(
            detect(&PathBuf::from("untitled.png"), &img),
            Some(Grid::new(8, 8))
        );
    }

    #[test]
    fn detect_non_divisible_grid_no_prior() {
        // 5×5 on a 512² canvas → 102.4px cells (512 not divisible by 5). The period detector must
        // find it from content with no filename token — the real-world VFX case (a 5×5 sheet on a
        // power-of-two atlas) that a divisor-only scan structurally cannot detect.
        let img = moving_dot_sheet_frac(5, 5, 512, 512);
        assert_eq!(
            detect(&PathBuf::from("untitled.png"), &img),
            Some(Grid::new(5, 5))
        );
    }

    #[test]
    fn detect_single_object_no_prior_none() {
        // One centred blob filling the frame (a lone sprite, no grid) → no repeating period → None.
        let img = img_from(256, 256, |x, y| {
            let dx = x as f32 - 128.0;
            let dy = y as f32 - 128.0;
            let v = (-(dx * dx + dy * dy) / (2.0 * 70.0 * 70.0)).exp();
            (v * 255.0) as u8
        });
        assert_eq!(detect(&PathBuf::from("spark.png"), &img), None);
    }

    #[test]
    fn detect_content_beats_wrong_token() {
        // Content is the determining factor: an 8×8 sheet with a filename lying about 4×4 detects
        // as 8×8 from the pixels — the token never overrides content.
        let img = moving_dot_sheet(8, 8, 16);
        assert_eq!(
            detect(&PathBuf::from("boom_4x4.png"), &img),
            Some(Grid::new(8, 8))
        );
    }

    #[test]
    fn detect_horizontal_strip_no_prior() {
        // A 4×1 strip: four full-height bars, one per cell, centred (clean column period) and
        // widening frame to frame (similar neighbours, distinct ends). No vertical structure, so
        // the row axis has no period → a strip. Content must detect 4×1 with no filename token.
        let (cols, cw, h) = (4u32, 56u32, 168u32);
        let img = img_from(cols * cw, h, |x, _y| {
            let c = (x / cw) as f32;
            let lx = (x % cw) as f32 - cw as f32 * 0.5;
            let sig = cw as f32 * (0.12 + 0.04 * c); // width grows per frame
            ((-(lx * lx) / (2.0 * sig * sig)).exp() * 255.0) as u8
        });
        assert_eq!(
            detect(&PathBuf::from("untitled.png"), &img),
            Some(Grid::new(4, 1))
        );
    }

    #[test]
    fn detect_dissimilar_bands_not_strip() {
        // Three horizontally-uniform bands (up-ramp, down-ramp, flat-ish) split by dark gutters:
        // the row axis reads a period-3, but the bands don't resemble each other (a segmented
        // object, like a bat wing), so the strip adjacency guard rejects it.
        let (w, h) = (128u32, 96u32); // 3 bands of 32
        let img = img_from(w, h, |_x, y| {
            let ly = y % 32;
            if ly < 3 {
                return 0; // dark gutter → row period 3, columns stay uniform (no col period)
            }
            match y / 32 {
                0 => (ly * 7) as u8,            // ramp up
                1 => ((32 - ly) * 7) as u8,     // ramp down — anti-correlated with band 0
                _ => (60 + (ly % 5) * 8) as u8, // different again
            }
        });
        assert_eq!(detect(&PathBuf::from("wing.png"), &img), None);
    }

    #[test]
    fn detect_flat_is_none() {
        let img = img_from(128, 128, |_, _| 128); // solid grey
        assert_eq!(detect(&PathBuf::from("untitled.png"), &img), None);
    }

    #[test]
    fn detect_fine_tile_is_none() {
        // A fine repeating checkerboard is a tiled texture, not an animation.
        let img = img_from(128, 128, |x, y| {
            if ((x / 4) + (y / 4)) % 2 == 0 {
                30
            } else {
                220
            }
        });
        assert_eq!(detect(&PathBuf::from("untitled.png"), &img), None);
    }

    #[test]
    fn detect_padded_sheet_with_prior() {
        // 4×4 blob sheet with a dark padding gap between cells; prior agrees.
        let cell = 20u32;
        let (cols, rows) = (4u32, 4u32);
        let img = img_from(cols * cell, rows * cell, |x, y| {
            let lx = x % cell;
            let ly = y % cell;
            // 2px dark gap around each cell → clear boundary anomaly.
            if lx < 2 || ly < 2 || lx >= cell - 2 || ly >= cell - 2 {
                return 0;
            }
            let frame = (y / cell) * cols + (x / cell);
            let fx = (lx as f32) / cell as f32;
            let fy = (ly as f32) / cell as f32;
            ((((fx + fy) * 3.0 + frame as f32 * 0.5).sin() * 0.5 + 0.5) * 255.0) as u8
        });
        assert_eq!(
            detect(&PathBuf::from("pad_4x4.png"), &img),
            Some(Grid::new(4, 4))
        );
    }

    #[test]
    fn detect_weak_content_prior_fallback() {
        // A smooth gradient (no grid structure) with an explicit token → fall back to the token.
        let img = img_from(128, 64, |x, _| (x * 255 / 128) as u8);
        assert_eq!(
            detect(&PathBuf::from("grad_4x2.png"), &img),
            Some(Grid::new(4, 2))
        );
    }

    #[test]
    fn detect_weak_content_no_token_none() {
        let img = img_from(128, 64, |x, _| (x * 255 / 128) as u8);
        assert_eq!(detect(&PathBuf::from("grad.png"), &img), None);
    }

    #[test]
    fn detect_alpha_only_sheet() {
        // A sheet whose structure lives ONLY in alpha — RGB is solid white, the moving blob is the
        // alpha mask (a common VFX export, e.g. steam). The luma pass sees a flat field; the alpha
        // fallback must recover the 8×8 grid. No filename token.
        let (cols, rows, cell) = (8u32, 8u32, 16u32);
        let (w, h) = (cols * cell, rows * cell);
        let mut px = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let c = x / cell;
                let r = y / cell;
                let frame = (r * cols + c) as f32;
                let lx = (x % cell) as f32;
                let ly = (y % cell) as f32;
                let bx = cell as f32 * (0.5 + 0.2 * (frame * 0.9).sin());
                let by = cell as f32 * (0.5 + 0.2 * (frame * 1.3).cos());
                let d2 = (lx - bx).powi(2) + (ly - by).powi(2);
                let sig = cell as f32 * 0.15;
                let a = ((-d2 / (2.0 * sig * sig)).exp() * 255.0) as u8;
                px.extend_from_slice(&[255, 255, 255, a]); // flat white RGB, structure in alpha
            }
        }
        let img = DecodedImage {
            pixels: px,
            width: w,
            height: h,
            format: PixelFormat::Rgba8Unorm,
            bit_depth: 8,
            channels: 4,
            alpha_opaque: false, // gate the alpha pass on
            icc: None,
            source_format: "TEST",
            downscaled_from: None,
            animation: None,
        };
        assert_eq!(
            detect(&PathBuf::from("steam.png"), &img),
            Some(Grid::new(8, 8))
        );
    }
}
