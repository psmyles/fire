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
//!   * grid auto-detection ([`detect`]) run off-thread on the decode worker — a filename token
//!     like `_8x8` is only a *prior* that content analysis must validate, surfaced solely as a
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
/// Minimum cell size (source px) for a divisor to be a candidate.
const CELL_MIN_PX: u32 = 8;
/// Minimum cell size (thumbnail px) required to score a candidate at all.
const THUMB_CELL_MIN: u32 = 4;
/// Divisor sweep per axis (plus the filename prior, admitted even above this).
const DIV_MIN: u32 = 2;
const DIV_MAX: u32 = 16;

/// Weight of the (per-axis) boundary-anomaly term relative to the shift-correlation term in an
/// axis's evidence. Shift-correlation carries full-bleed sheets; boundary anomaly carries padded
/// or hard-seamed sheets of dissimilar frames.
const BA_WEIGHT: f32 = 0.4;
/// Absolute combined-score bar a non-prior candidate must clear to be a confident detection.
const SCORE_ABS_MIN: f32 = 1.0;
/// Lower bar the explicit filename prior must clear to be considered validated.
const PRIOR_SCORE_MIN: f32 = 0.5;
/// A confident non-prior winner must beat the best non-harmonic competitor by this factor.
const NONPRIOR_MARGIN: f32 = 1.5;
/// Small denominator guard for the relative boundary-anomaly ratio.
const BOUNDARY_EPS: f32 = 1e-3;
/// Variance (on 0..1 luma) below which a cell is "flat" and excluded from similarity scoring.
const FLAT_VAR_EPS: f32 = 1e-4;
/// Minimum fraction of non-flat cells for a candidate to be scored — a finer grid that splits a
/// localized sprite into mostly-empty cells fails this and is rejected (over-split guard).
const MIN_NONFLAT_FRAC: f32 = 0.5;
/// Consecutive-cell median NCC at/above which cells are treated as identical (a tiled or
/// gradient texture, not distinct animation frames) and rejected.
const IDENTICAL_CAP: f32 = 0.97;
/// Within a harmonic family, keep the coarser grid over a finer one unless the finer scores
/// clearly higher: drop the finer if the coarser scores at least this fraction of it (resists
/// over-splitting a true grid into a finer harmonic).
const COARSE_PREFER: f32 = 0.9;

/// Detect the flipbook grid of a decoded sheet, or `None`. The filename token is a *prior* that
/// content analysis validates or overrides; content can also detect a grid with no token at all.
/// Runs on the decode worker; a few ms of pure Rust over a ≤512px grayscale copy.
pub fn detect(path: &Path, image: &DecodedImage) -> Option<Grid> {
    let prior = filename_prior(path);
    // Content detection wins; if it finds nothing, fall back to an explicit filename token (a
    // harmless hint reflecting the author's stated intent).
    content_grid(image, prior).or(prior)
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

/// Content analysis: validate/override the prior, or detect from scratch. See the module-level
/// resolution rules; returns `None` when nothing clears its bar (the prior fallback lives in
/// [`detect`]).
fn content_grid(image: &DecodedImage, prior: Option<Grid>) -> Option<Grid> {
    let (thumb, tw, th) = luminance_thumbnail(image)?;
    let (w, h) = (image.width, image.height);

    let col_divs = axis_divisors(w, tw, prior.map(|g| g.cols));
    let row_divs = axis_divisors(h, th, prior.map(|g| g.rows));
    if col_divs.is_empty() || row_divs.is_empty() {
        return None;
    }

    // Per-axis evidence: gradient-activity profiles (for boundary anomaly) and shift-correlation
    // (for content periodicity) as a function of the number of divisions.
    let col_act = axis_activity(&thumb, tw, th, Axis::Col);
    let row_act = axis_activity(&thumb, tw, th, Axis::Row);
    let col_median = median(&col_act);
    let row_median = median(&row_act);
    let col_ev = |n: u32| {
        axis_evidence(
            &col_act,
            col_median,
            shift_corr(&thumb, tw, th, Axis::Col, n),
            n,
        )
    };
    let row_ev = |n: u32| {
        axis_evidence(
            &row_act,
            row_median,
            shift_corr(&thumb, tw, th, Axis::Row, n),
            n,
        )
    };

    // Score every (cols × rows) candidate.
    let mut scored: Vec<(Grid, f32)> = Vec::new();
    for &cols in &col_divs {
        let cx = col_ev(cols);
        for &rows in &row_divs {
            let grid = Grid::new(cols, rows);
            if grid.cells() < 4 {
                continue;
            }
            if cols < 2 && rows < 2 {
                continue;
            }
            let (sim, nonflat_frac) = cell_similarity(&thumb, tw, th, grid);
            // A flipbook needs animation-like cells: enough non-flat cells that are NOT identical.
            // Identical cells (median NCC ≥ cap) are a tiling/gradient, not distinct frames; a
            // mostly-flat split is an over-split of a localized sprite. Both are rejected here so
            // periodicity alone (shift-correlation) can't promote a tiled texture.
            let animationlike = nonflat_frac >= MIN_NONFLAT_FRAC && sim < IDENTICAL_CAP;
            let score = if animationlike {
                cx + row_ev(rows)
            } else {
                0.0
            };
            scored.push((grid, score));
        }
    }
    if scored.is_empty() {
        return None;
    }

    let winner = confident_winner(&scored);

    match prior {
        Some(p) => {
            // A confident detection that differs from the prior overrides it (filename wrong or
            // coarser than the real grid). Otherwise a validated prior wins.
            if let Some(w) = winner {
                if w != p {
                    return Some(w);
                }
                return Some(p);
            }
            let prior_score = scored
                .iter()
                .find(|(g, _)| *g == p)
                .map(|(_, s)| *s)
                .unwrap_or(0.0);
            if prior_score >= PRIOR_SCORE_MIN {
                Some(p)
            } else {
                None // detect() falls back to the prior anyway
            }
        }
        None => winner,
    }
}

/// Pick the confident non-prior winner: the highest-scoring qualifier (biased toward the coarser
/// grid within a harmonic family) that clears the absolute bar and beats the best non-harmonic
/// competitor by [`NONPRIOR_MARGIN`]. `None` if nothing qualifies.
fn confident_winner(scored: &[(Grid, f32)]) -> Option<Grid> {
    let qualifiers: Vec<(Grid, f32)> = scored
        .iter()
        .copied()
        .filter(|(_, s)| *s >= SCORE_ABS_MIN)
        .collect();
    if qualifiers.is_empty() {
        return None;
    }
    // Collapse harmonic families toward the coarser grid: drop a finer qualifier when a coarser
    // harmonic scores nearly as well (a finer harmonic re-splits the true grid's cells and tends
    // to inflate similarity). A finer grid survives only if it scores clearly higher.
    let kept: Vec<(Grid, f32)> = qualifiers
        .iter()
        .copied()
        .filter(|&(g, gs)| {
            !qualifiers.iter().any(|&(o, os)| {
                o != g && harmonic(g, o) && o.cells() < g.cells() && os >= COARSE_PREFER * gs
            })
        })
        .collect();

    let (winner, wscore) = kept
        .iter()
        .copied()
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))?;

    // Margin over the best competitor not harmonically related to the winner.
    let competitor = scored
        .iter()
        .filter(|&&(g, _)| g != winner && !harmonic(g, winner))
        .map(|&(_, s)| s)
        .fold(0.0_f32, f32::max);
    if wscore >= NONPRIOR_MARGIN * competitor {
        Some(winner)
    } else {
        None
    }
}

/// Two grids are harmonically related when one is an integer multiple of the other on *both*
/// axes (so their seams nest); such candidates naturally co-score and are excluded from the
/// competitor margin.
fn harmonic(a: Grid, b: Grid) -> bool {
    let multiple =
        |x: Grid, y: Grid| x.cols.is_multiple_of(y.cols) && x.rows.is_multiple_of(y.rows);
    multiple(a, b) || multiple(b, a)
}

/// Candidate divisors for one axis: `2..=16` that divide `dim` with cell ≥ [`CELL_MIN_PX`],
/// plus the `prior` value (admitted even above 16 when it divides and its thumbnail cell is
/// large enough), plus `1` (strip sheets — a 1 on one axis is only usable when the other is ≥ 2,
/// enforced during candidate scoring).
fn axis_divisors(dim: u32, thumb_dim: u32, prior: Option<u32>) -> Vec<u32> {
    let mut out = vec![1u32];
    for n in DIV_MIN..=DIV_MAX {
        if dim.is_multiple_of(n) && dim / n >= CELL_MIN_PX && thumb_dim / n >= THUMB_CELL_MIN {
            out.push(n);
        }
    }
    if let Some(p) = prior {
        if (GRID_MIN..=GRID_MAX).contains(&p)
            && dim.is_multiple_of(p)
            && thumb_dim / p.max(1) >= THUMB_CELL_MIN
            && !out.contains(&p)
        {
            out.push(p);
        }
    }
    out
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

/// Boundary-anomaly score for `n` divisions of an axis whose gradient-activity profile is `act`
/// (with precomputed `median`). Interior boundaries sit at `round(k·len/n)`; each contributes
/// the largest `|act − median| / (median + eps)` within ±1 gap (rounding tolerance). Padding
/// reads as a deep dip, a butted seam as a spike — both anomalous. `0` when `n < 2`.
fn boundary_score(act: &[f32], median: f32, n: u32) -> f32 {
    if n < 2 || act.is_empty() {
        return 0.0;
    }
    let len = act.len() + 1; // number of pixels along the axis
    let denom = median + BOUNDARY_EPS;
    let mut sum = 0.0;
    let mut count = 0u32;
    for k in 1..n {
        let pos = ((k as f32) * (len as f32) / (n as f32)).round() as i64; // boundary pixel
        let center = pos - 1; // gap between pixel pos-1 and pos
        let mut best = 0.0f32;
        for d in -1..=1 {
            let gi = center + d;
            if gi >= 0 && (gi as usize) < act.len() {
                let dev = (act[gi as usize] - median).abs() / denom;
                best = best.max(dev);
            }
        }
        sum += best;
        count += 1;
    }
    if count == 0 {
        0.0
    } else {
        sum / count as f32
    }
}

/// Combined evidence that an axis is divided into `n` cells: content periodicity (shift
/// correlation, clamped to ≥0) plus a weighted boundary-anomaly term. `0` for `n < 2` (no
/// division carries no evidence — a strip sheet leans on the other axis).
fn axis_evidence(act: &[f32], median: f32, shift_corr: f32, n: u32) -> f32 {
    if n < 2 {
        return 0.0;
    }
    shift_corr.max(0.0) + BA_WEIGHT * boundary_score(act, median, n)
}

/// Normalized cross-correlation (Pearson) between the thumbnail and itself shifted by one cell
/// (`dim/n`) along `axis`. Peaks at the true cell size (adjacent frames align) and its multiples;
/// a sub-cell shift misaligns content and scores low — so the maximum over an axis's divisors
/// lands on the true division. `0` for a degenerate shift.
fn shift_corr(thumb: &[f32], tw: u32, th: u32, axis: Axis, n: u32) -> f32 {
    if n < 2 {
        return 0.0;
    }
    let (tw, th) = (tw as usize, th as usize);
    let (dim, is_col) = match axis {
        Axis::Col => (tw, true),
        Axis::Row => (th, false),
    };
    let s = ((dim as f32) / (n as f32)).round() as usize;
    if s < 1 || s >= dim {
        return 0.0;
    }
    // Gather (a, b) pairs where b is a's neighbour one cell along the axis.
    let mut sa = 0.0f64;
    let mut sb = 0.0f64;
    let mut saa = 0.0f64;
    let mut sbb = 0.0f64;
    let mut sab = 0.0f64;
    let mut cnt = 0.0f64;
    if is_col {
        for y in 0..th {
            let row = y * tw;
            for x in 0..tw - s {
                let a = thumb[row + x] as f64;
                let b = thumb[row + x + s] as f64;
                sa += a;
                sb += b;
                saa += a * a;
                sbb += b * b;
                sab += a * b;
                cnt += 1.0;
            }
        }
    } else {
        for y in 0..th - s {
            let row = y * tw;
            let row2 = (y + s) * tw;
            for x in 0..tw {
                let a = thumb[row + x] as f64;
                let b = thumb[row2 + x] as f64;
                sa += a;
                sb += b;
                saa += a * a;
                sbb += b * b;
                sab += a * b;
                cnt += 1.0;
            }
        }
    }
    if cnt < 2.0 {
        return 0.0;
    }
    let cov = sab - sa * sb / cnt;
    let va = saa - sa * sa / cnt;
    let vb = sbb - sb * sb / cnt;
    if va <= 0.0 || vb <= 0.0 {
        return 0.0;
    }
    (cov / (va.sqrt() * vb.sqrt())) as f32
}

/// Cell-similarity score for a candidate grid: split the thumbnail into cells, box-downsample
/// each to an 8×8 thumbnail, mean-subtract and L2-normalize (skipping near-flat cells), then
/// take the median normalized cross-correlation between consecutive cells (row-major). Returns
/// `(median_ncc, non_flat_fraction)`. Animation frames are similar-but-not-identical (high
/// median NCC); an arbitrary image chopped into a grid scores low.
fn cell_similarity(thumb: &[f32], tw: u32, th: u32, grid: Grid) -> (f32, f32) {
    let cols = grid.cols.max(1);
    let rows = grid.rows.max(1);
    let n = (cols * rows) as usize;
    if n < 2 {
        return (0.0, 0.0);
    }
    // Normalized 8×8 descriptor per cell (None if flat).
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

    let mut nccs: Vec<f32> = Vec::with_capacity(n - 1);
    for i in 0..n - 1 {
        if let (Some(a), Some(b)) = (&descs[i], &descs[i + 1]) {
            let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
            nccs.push(dot); // descriptors are unit-norm → dot == NCC
        }
    }
    if nccs.is_empty() {
        return (0.0, nonflat_frac);
    }
    (median(&nccs), nonflat_frac)
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

/// Build the ≤512px grayscale (luminance, 0..1) analysis copy via integer box binning. Returns
/// `(pixels, thumb_w, thumb_h)`; `None` if the image is too small to analyze.
fn luminance_thumbnail(image: &DecodedImage) -> Option<(Vec<f32>, u32, u32)> {
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

    let mut out = vec![0.0f32; (tw * th) as usize];
    for oy in 0..th {
        for ox in 0..tw {
            let mut s = 0.0f32;
            let mut cnt = 0u32;
            for yy in 0..b {
                let sy = oy * b + yy;
                if sy >= h {
                    break;
                }
                for xx in 0..b {
                    let sx = ox * b + xx;
                    if sx >= w {
                        break;
                    }
                    let off = sy as usize * stride + sx as usize * bpp;
                    s += luma_at(px, off, image.format);
                    cnt += 1;
                }
            }
            out[(oy * tw + ox) as usize] = if cnt > 0 { s / cnt as f32 } else { 0.0 };
        }
    }
    Some((out, tw, th))
}

/// Rec.709 luminance (0..1-ish; HDR values may exceed 1, which is fine — scores are relative) of
/// the RGBA pixel at byte offset `off`, per format. Mirrors the per-format lane handling in
/// `fire_decode`'s `alpha_is_opaque`.
fn luma_at(px: &[u8], off: usize, format: PixelFormat) -> f32 {
    let (r, g, b) = match format {
        PixelFormat::Rgba8Unorm => (
            px[off] as f32 / 255.0,
            px[off + 1] as f32 / 255.0,
            px[off + 2] as f32 / 255.0,
        ),
        PixelFormat::Rgba16Unorm => (
            u16::from_ne_bytes([px[off], px[off + 1]]) as f32 / 65535.0,
            u16::from_ne_bytes([px[off + 2], px[off + 3]]) as f32 / 65535.0,
            u16::from_ne_bytes([px[off + 4], px[off + 5]]) as f32 / 65535.0,
        ),
        PixelFormat::Rgba16Float => (
            half_to_f32(u16::from_ne_bytes([px[off], px[off + 1]])),
            half_to_f32(u16::from_ne_bytes([px[off + 2], px[off + 3]])),
            half_to_f32(u16::from_ne_bytes([px[off + 4], px[off + 5]])),
        ),
        PixelFormat::Rgba32Float => (
            f32::from_ne_bytes([px[off], px[off + 1], px[off + 2], px[off + 3]]),
            f32::from_ne_bytes([px[off + 4], px[off + 5], px[off + 6], px[off + 7]]),
            f32::from_ne_bytes([px[off + 8], px[off + 9], px[off + 10], px[off + 11]]),
        ),
    };
    0.2126 * r + 0.7152 * g + 0.0722 * b
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

    #[test]
    fn detect_moving_dot_no_prior() {
        let img = moving_dot_sheet(8, 8, 16); // 128×128, full-bleed
        assert_eq!(
            detect(&PathBuf::from("untitled.png"), &img),
            Some(Grid::new(8, 8))
        );
    }

    #[test]
    fn detect_overrides_wrong_prior() {
        // Same 8×8 content, filename lies about 4×4 → content overrides to 8×8.
        let img = moving_dot_sheet(8, 8, 16);
        assert_eq!(
            detect(&PathBuf::from("boom_4x4.png"), &img),
            Some(Grid::new(8, 8))
        );
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
}
