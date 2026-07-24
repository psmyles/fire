//! View-transform + display state and the pure geometry behind pan/zoom/fit.
//!
//! Everything here is windowing- and render-free so it can be unit-tested: the
//! zoom-to-cursor fixed point, fit centering, the pan clamp, and the screen↔image
//! round-trip. [`crate::render::gpu`] feeds this state into the pixel shader's constant
//! buffer to map each output pixel back into the source image.

/// Zoom bounds (screen pixels per image pixel). 0.02 lets a huge image shrink to a
/// thumbnail; 64× is enough texel-peeping for a viewer.
pub const MIN_ZOOM: f32 = 0.02;
pub const MAX_ZOOM: f32 = 64.0;

/// Letterbox gutter (physical px per side) reserved by fit-to-window. Fitting shrinks the
/// image to the surface *minus this inset on every edge*, guaranteeing room for the 1px
/// screen-space image outline (drawn just outside the boundary) on the constraining axis too.
pub const FIT_GUTTER: f32 = 1.0;

/// The drawable image surface — the image's sub-rect of the window (`App::image_rect`), in
/// physical px, *not* the whole client area: the toolbar, status bar and transport band are drawn
/// over the rest of the same swapchain. So this is exactly the image region and carries no chrome
/// insets, and the pan/zoom/fit math below is written against it alone.
#[derive(Clone, Copy, Debug)]
pub struct Viewport {
    pub width: f32,
    pub height: f32,
}

impl Viewport {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width: width.max(1) as f32,
            height: height.max(1) as f32,
        }
    }

    /// Center of the surface in pixels (origin top-left, y down).
    pub fn center(&self) -> (f32, f32) {
        (self.width * 0.5, self.height * 0.5)
    }
}

/// Channel-isolation mode (selects the per-pixel branch in the [`crate::render::gpu`] shader).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    Rgb,
    R,
    G,
    B,
    A,
}

/// HDR tonemap operator (applies to float sources only, #13).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tonemap {
    Reinhard,
    Aces,
}

/// Viewport backdrop: fills the letterbox around the image and shows through transparent pixels.
/// The default tracks the image (opaque → [`Background::Black`], has-alpha → [`Background::Checker`]);
/// the toolbar's right-side group overrides it. Maps to the `background` branch in the shader.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Background {
    Black,
    White,
    /// Neutral 40% grey (sRGB).
    Grey,
    /// Photoshop-style light/dark checkerboard (the transparency indicator).
    Checker,
}

/// Non-geometric display state, reset to neutral for each new file (#17).
#[derive(Clone, Copy, Debug)]
pub struct DisplayState {
    pub channel: Channel,
    /// Exposure in stops; multiplies linear color by `2^exposure` (HDR sources only).
    pub exposure: f32,
    pub tonemap: Tonemap,
}

impl Default for DisplayState {
    fn default() -> Self {
        Self {
            channel: Channel::Rgb,
            exposure: 0.0,
            tonemap: Tonemap::Reinhard,
        }
    }
}

/// Geometric view state. `zoom` is screen px per image px (1.0 = 1:1). `pan` is the
/// image-center offset from the surface center, in surface pixels (so pan 0 =
/// centered). `fit` records that we're in fit mode, so a resize re-fits rather than
/// keeping a stale zoom.
#[derive(Clone, Copy, Debug)]
pub struct ViewState {
    pub zoom: f32,
    pub pan: (f32, f32),
    pub fit: bool,
    /// While in fit mode, whether the current fit also scales small images *up* to fill the
    /// surface. Remembered so a window resize re-fits with the same rule the active fit used: the
    /// per-image load fits *without* upscaling (small images open at 1:1), while the explicit
    /// "fit to window" command can fill the surface. Meaningless when `fit` is false.
    pub fit_upscale: bool,
}

impl Default for ViewState {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            pan: (0.0, 0.0),
            fit: true,
            fit_upscale: false,
        }
    }
}

impl ViewState {
    /// Fit the whole image within the surface, centered. When `upscale` is false a small image is
    /// left at native 1:1 (the texture-viewer convention) rather than blown up into a blur — zoom in
    /// explicitly to go past 100%; when true the image is also scaled *up* to fill the surface, so
    /// fit always means "fit to window" regardless of the image's size (the `fit-upscale` config key).
    pub fn fit_to_window(&mut self, image: (u32, u32), vp: &Viewport, upscale: bool) {
        // Fit into the surface minus a 1px gutter on every side so the outside outline always has room.
        let (uw, uh) = (
            (vp.width - 2.0 * FIT_GUTTER).max(1.0),
            (vp.height - 2.0 * FIT_GUTTER).max(1.0),
        );
        let (iw, ih) = (image.0.max(1) as f32, image.1.max(1) as f32);
        let z = (uw / iw).min(uh / ih);
        // Without upscaling, never go past 1:1 (a small image already sits inside the surface).
        let z = if upscale { z } else { z.min(1.0) };
        self.zoom = z.clamp(MIN_ZOOM, MAX_ZOOM);
        self.pan = (0.0, 0.0);
        self.fit = true;
        self.fit_upscale = upscale;
    }

    /// 1:1 — one image pixel per surface pixel, centered.
    pub fn one_to_one(&mut self) {
        self.zoom = 1.0;
        self.pan = (0.0, 0.0);
        self.fit = false;
    }

    /// Multiply zoom by `factor` about `cursor` (surface px), keeping the image point
    /// currently under the cursor fixed on screen. Manual zoom leaves fit mode.
    pub fn zoom_to_cursor(
        &mut self,
        factor: f32,
        cursor: (f32, f32),
        image: (u32, u32),
        vp: &Viewport,
    ) {
        self.zoom_to(self.zoom * factor, cursor, image, vp);
    }

    /// As [`Self::zoom_to_cursor`] but for an absolute target zoom — what the snapping scrubby-zoom
    /// drag works in, since a detent names the zoom it wants rather than a step to take. A target
    /// that doesn't move the zoom (already there, or clamped against a bound) is a no-op, so
    /// holding a drag inside a detent can't drift the pan.
    pub fn zoom_to(&mut self, zoom: f32, cursor: (f32, f32), image: (u32, u32), vp: &Viewport) {
        let old = self.zoom;
        let new = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        if new == old {
            return;
        }
        // Keep the image point under the cursor fixed: solve for the new pan so the
        // cursor's offset-from-image-center in image pixels is unchanged across the zoom.
        let c = vp.center();
        let rel = (cursor.0 - c.0, cursor.1 - c.1);
        let ratio = new / old;
        self.pan = (
            rel.0 - (rel.0 - self.pan.0) * ratio,
            rel.1 - (rel.1 - self.pan.1) * ratio,
        );
        self.zoom = new;
        self.fit = false;
        self.clamp_pan(image, vp);
    }

    /// Zoom about the surface center (keyboard zoom).
    pub fn zoom_centered(&mut self, factor: f32, image: (u32, u32), vp: &Viewport) {
        let c = vp.center();
        self.zoom_to_cursor(factor, c, image, vp);
    }

    /// Drag the image by a surface-pixel delta (mouse pan). Leaves fit mode.
    pub fn pan_by(&mut self, delta: (f32, f32), image: (u32, u32), vp: &Viewport) {
        self.pan = (self.pan.0 + delta.0, self.pan.1 + delta.1);
        self.fit = false;
        self.clamp_pan(image, vp);
    }

    /// On-screen image size in surface pixels.
    pub fn image_screen_size(&self, image: (u32, u32)) -> (f32, f32) {
        (image.0 as f32 * self.zoom, image.1 as f32 * self.zoom)
    }

    /// Bound the pan so the image stays *reachable* without trapping it inside the surface: you
    /// can pan until the image is *just* fully off any edge — `(image_screen + surface) / 2` per
    /// axis, symmetric about centered — but no further, so it can be pushed out of the frame yet
    /// never flung infinitely into the void. Fit (`F`) or 1:1 recenters it.
    pub fn clamp_pan(&mut self, image: (u32, u32), vp: &Viewport) {
        let (uw, uh) = (vp.width, vp.height);
        let (sw, sh) = self.image_screen_size(image);
        let lim_x = (sw + uw) * 0.5;
        let lim_y = (sh + uh) * 0.5;
        self.pan = (
            self.pan.0.clamp(-lim_x, lim_x),
            self.pan.1.clamp(-lim_y, lim_y),
        );
    }

    /// Map a surface-pixel position to image pixel coordinates (origin top-left). Inverse
    /// of [`Self::image_to_screen`]; the eyedropper (Phase 4) reads pixels through this.
    #[allow(dead_code)] // wired to the pixel inspector in Phase 4; unit-tested now
    pub fn screen_to_image(
        &self,
        screen: (f32, f32),
        image: (u32, u32),
        vp: &Viewport,
    ) -> (f32, f32) {
        let c = vp.center();
        let img_center = (c.0 + self.pan.0, c.1 + self.pan.1);
        let off = (screen.0 - img_center.0, screen.1 - img_center.1);
        (
            image.0 as f32 * 0.5 + off.0 / self.zoom,
            image.1 as f32 * 0.5 + off.1 / self.zoom,
        )
    }

    /// Map image pixel coordinates back to a surface-pixel position.
    pub fn image_to_screen(&self, img: (f32, f32), image: (u32, u32), vp: &Viewport) -> (f32, f32) {
        let c = vp.center();
        let img_center = (c.0 + self.pan.0, c.1 + self.pan.1);
        let off = (
            (img.0 - image.0 as f32 * 0.5) * self.zoom,
            (img.1 - image.1 as f32 * 0.5) * self.zoom,
        );
        (img_center.0 + off.0, img_center.1 + off.1)
    }
}

/// The detent state of one in-flight scrubby-zoom drag: which snap the zoom is pinned to, and how
/// far the drag has pushed past it. Lives for the length of a gesture and starts fresh on the next.
///
/// Crossing a snap pins the zoom there and swallows the next `release` of drag; past that the zoom
/// resumes *from the snap*, so the gesture stays continuous — no jump on the way out of a detent —
/// and a drag held down keeps zooming on through snap after snap. A step that clears a snap by more
/// than the release distance (a fast scrub) passes straight through: detents notch a deliberate
/// drag without braking a flick.
///
/// The snap levels and the release distance both come from the config (`zoom-snap-levels` /
/// `zoom-snap`), so this type holds no ladder of its own.
#[derive(Clone, Copy, Debug, Default)]
pub struct ZoomDetent {
    held: Option<Held>,
}

/// A snap the drag is currently sitting in, both fields in natural-log zoom units: the snapped zoom
/// and the signed travel accumulated since it engaged.
#[derive(Clone, Copy, Debug)]
struct Held {
    snap: f32,
    travel: f32,
}

impl ZoomDetent {
    /// Advance a drag by `step` (natural-log zoom units — drag px × sensitivity) from the current
    /// `zoom` and return the zoom to move to.
    ///
    /// `snaps` are the zoom levels to detent on (screen px per image px) and `release` is how far
    /// past one the drag must travel to break out, in the same log units as `step`. An empty ladder
    /// or a non-positive `release` is snapping switched off, and the drag runs free.
    pub fn step(&mut self, zoom: f32, step: f32, snaps: &[f32], release: f32) -> f32 {
        let from = zoom.max(MIN_ZOOM).ln();
        if release <= 0.0 || snaps.is_empty() {
            self.held = None;
            return (from + step).exp();
        }
        if let Some(mut held) = self.held {
            held.travel += step;
            if held.travel.abs() <= release {
                self.held = Some(held);
                return held.snap.exp();
            }
            // Broken out: resume from the snap, minus the travel spent holding it there.
            self.held = None;
            return (held.snap + held.travel - release * held.travel.signum()).exp();
        }
        let to = from + step;
        match crossed(from, to, snaps) {
            Some(snap) if (to - snap).abs() <= release => {
                self.held = Some(Held {
                    snap,
                    travel: to - snap,
                });
                snap.exp()
            }
            _ => to.exp(),
        }
    }

    /// Forget any held snap. Call when a gesture begins, so a fresh drag can't inherit the detent
    /// the last one was sitting in.
    pub fn reset(&mut self) {
        self.held = None;
    }
}

/// The last snap (log units) crossed moving `from` → `to`, i.e. the one nearest where the step
/// lands — so a big step that happens to end beside a snap still catches it, rather than being
/// judged against the first rung it flew past.
fn crossed(from: f32, to: f32, snaps: &[f32]) -> Option<f32> {
    let up = to > from;
    snaps
        .iter()
        .copied()
        .filter(|z| *z > 0.0)
        .map(f32::ln)
        .filter(|s| {
            if up {
                *s > from && *s <= to
            } else {
                *s < from && *s >= to
            }
        })
        .reduce(|a, b| if up { a.max(b) } else { a.min(b) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vp() -> Viewport {
        Viewport::new(1000, 800)
    }

    #[test]
    fn fit_shrinks_large_image_and_caps_small_at_one_to_one() {
        let v = vp();
        // Large image: limited by the tighter axis, fitting into the surface minus a 1px
        // gutter per side. 998/4000 = 0.2495 (width) is smaller than 798/2000 = 0.399
        // (height), so width constrains the fit. A large image shrinks the same with or
        // without upscaling.
        let mut large = ViewState::default();
        large.fit_to_window((4000, 2000), &v, false);
        assert!((large.zoom - 0.2495).abs() < 1e-6, "zoom = {}", large.zoom);
        assert_eq!(large.pan, (0.0, 0.0));
        assert!(large.fit);
        // Small image, no upscale: capped at 1:1, never enlarged.
        let mut small = ViewState::default();
        small.fit_to_window((100, 100), &v, false);
        assert_eq!(small.zoom, 1.0);
        assert!(!small.fit_upscale, "fit records that it did not upscale");
    }

    #[test]
    fn fit_upscales_small_image_to_fill_the_window() {
        let v = vp();
        // Small image with upscaling on: scaled up to fill the surface (minus the gutter),
        // constrained by the tighter axis. 998/100 = 9.98 (width) vs 798/100 = 7.98 (height),
        // so height constrains and the image is enlarged ~8×.
        let mut small = ViewState::default();
        small.fit_to_window((100, 100), &v, true);
        assert!((small.zoom - 7.98).abs() < 1e-6, "zoom = {}", small.zoom);
        assert_eq!(small.pan, (0.0, 0.0));
        assert!(small.fit);
        assert!(small.fit_upscale, "fit records that it upscaled");
        // A tiny image's fit zoom is still clamped to MAX_ZOOM.
        let mut tiny = ViewState::default();
        tiny.fit_to_window((1, 1), &v, true);
        assert_eq!(tiny.zoom, MAX_ZOOM);
    }

    #[test]
    fn zoom_to_cursor_keeps_point_under_cursor_fixed() {
        let v = vp();
        let image = (2000u32, 1500u32);
        let mut s = ViewState::default();
        s.fit_to_window(image, &v, false);
        let cursor = (700.0, 300.0);
        // The image pixel under the cursor before zooming...
        let before = s.screen_to_image(cursor, image, &v);
        s.zoom_to_cursor(2.5, cursor, image, &v);
        // ...must still be under the cursor after (within float tolerance, modulo clamp).
        let after = s.screen_to_image(cursor, image, &v);
        assert!(
            (before.0 - after.0).abs() < 0.5,
            "x: {} vs {}",
            before.0,
            after.0
        );
        assert!(
            (before.1 - after.1).abs() < 0.5,
            "y: {} vs {}",
            before.1,
            after.1
        );
        assert!(!s.fit);
    }

    #[test]
    fn screen_image_round_trip() {
        let v = vp();
        let image = (1234u32, 567u32);
        let mut s = ViewState {
            zoom: 1.7,
            pan: (-30.0, 45.0),
            fit: false,
            fit_upscale: false,
        };
        s.clamp_pan(image, &v);
        for &p in &[(0.0, 0.0), (640.0, 360.0), (999.0, 799.0)] {
            let img = s.screen_to_image(p, image, &v);
            let back = s.image_to_screen(img, image, &v);
            assert!((p.0 - back.0).abs() < 1e-3, "x {} -> {}", p.0, back.0);
            assert!((p.1 - back.1).abs() < 1e-3, "y {} -> {}", p.1, back.1);
        }
    }

    #[test]
    fn pan_clamp_lets_you_push_image_just_out_of_view() {
        let v = vp();
        let image = (4000u32, 800u32); // wider than the 1000px viewport, same height
        let mut s = ViewState {
            zoom: 1.0,
            pan: (0.0, 0.0),
            fit: false,
            fit_upscale: false,
        };
        // Pan far right; clamp pins it to (image_screen + surface)/2 — the image is then just
        // fully off the left edge and can't be pushed further.
        s.pan_by((100_000.0, 0.0), image, &v);
        let lim_x = (4000.0 + 1000.0) * 0.5;
        assert!((s.pan.0 - lim_x).abs() < 1e-6, "pan.x = {}", s.pan.0);
        // Even when the image is exactly the viewport height, it can now be pushed fully off
        // vertically (previously this axis had zero pan room).
        s.pan_by((0.0, 100_000.0), image, &v);
        let lim_y = (800.0 + 800.0) * 0.5;
        assert!((s.pan.1 - lim_y).abs() < 1e-6, "pan.y = {}", s.pan.1);
    }

    /// A ladder to detent against, independent of whatever the shipped config defaults to.
    const SNAPS: &[f32] = &[0.5, 1.0, 1.5, 2.0, 3.0, 4.0];

    /// ~2px of drag at the default scrubby-zoom sensitivity — a step small enough that the detent
    /// engages rather than being flown past.
    const NUDGE: f32 = 0.02;

    /// The default break-out distance, in the log units `step` works in (~12px of drag).
    const RELEASE: f32 = 0.12;

    #[test]
    fn scrubby_zoom_sticks_at_a_snap_then_carries_on() {
        let mut d = ZoomDetent::default();
        // Drag up from 90% until the detent catches 1:1.
        let mut zoom = 0.9;
        for _ in 0..6 {
            zoom = d.step(zoom, NUDGE, SNAPS, RELEASE);
        }
        assert!((zoom - 1.0).abs() < 1e-6, "zoom = {zoom}");
        // Keep dragging: it holds at 100% while the release distance is eaten...
        for _ in 0..3 {
            zoom = d.step(zoom, NUDGE, SNAPS, RELEASE);
            assert!((zoom - 1.0).abs() < 1e-6, "zoom = {zoom}");
        }
        // ...then resumes *from* the snap, so the zoom neither jumps out of the detent nor stalls
        // in it, and stops short of the next rung (150%).
        for _ in 0..10 {
            zoom = d.step(zoom, NUDGE, SNAPS, RELEASE);
        }
        assert!(zoom > 1.0 && zoom < 1.5, "zoom = {zoom}");
    }

    #[test]
    fn a_fast_scrub_passes_between_snaps() {
        let mut d = ZoomDetent::default();
        // ~90px of drag in one move, from 100% to a zoom that lands clear of 200% and 300%: the
        // snaps it flew over must not brake it.
        let zoom = d.step(1.0, 0.9, SNAPS, RELEASE);
        assert!((zoom - 0.9_f32.exp()).abs() < 1e-6, "zoom = {zoom}");
        // But a fast scrub that happens to *land* on a snap still detents there.
        let mut d = ZoomDetent::default();
        let zoom = d.step(1.0, 2.0_f32.ln() + 0.01, SNAPS, RELEASE);
        assert!((zoom - 2.0).abs() < 1e-6, "zoom = {zoom}");
    }

    /// Snapping switched off in the settings — no ladder, or no break-out distance — is a plain
    /// exponential scrub, exactly as it was before detents existed.
    #[test]
    fn snapping_off_zooms_straight_through() {
        for (snaps, release) in [(SNAPS, 0.0), (&[][..], RELEASE)] {
            let mut d = ZoomDetent::default();
            let mut zoom = 0.9;
            for _ in 0..20 {
                zoom = d.step(zoom, NUDGE, snaps, release);
            }
            let expect = 0.9 * (NUDGE * 20.0).exp();
            assert!((zoom - expect).abs() < 1e-5, "zoom = {zoom}, want {expect}");
        }
    }

    #[test]
    fn one_to_one_centers_at_unit_zoom() {
        let mut s = ViewState::default();
        s.fit_to_window((4000, 4000), &vp(), false);
        s.one_to_one();
        assert_eq!(s.zoom, 1.0);
        assert_eq!(s.pan, (0.0, 0.0));
        assert!(!s.fit);
    }
}
