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

/// The drawable image surface — the child view window's client area, in physical px. The
/// frame paints the toolbar/status chrome in separate windows, so the surface is exactly
/// the image region; there are no chrome insets to carry.
#[derive(Clone, Copy, Debug)]
pub struct Viewport {
    pub width: f32,
    pub height: f32,
}

impl Viewport {
    pub fn new(width: u32, height: u32) -> Self {
        Self { width: width.max(1) as f32, height: height.max(1) as f32 }
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
        Self { channel: Channel::Rgb, exposure: 0.0, tonemap: Tonemap::Reinhard }
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
        Self { zoom: 1.0, pan: (0.0, 0.0), fit: true, fit_upscale: false }
    }
}

impl ViewState {
    /// Fit the whole image within the surface, centered. When `upscale` is false a small image is
    /// left at native 1:1 (the texture-viewer convention) rather than blown up into a blur — zoom in
    /// explicitly to go past 100%; when true the image is also scaled *up* to fill the surface, so
    /// fit always means "fit to window" regardless of the image's size (the `fit-upscale` config key).
    pub fn fit_to_window(&mut self, image: (u32, u32), vp: &Viewport, upscale: bool) {
        // Fit into the surface minus a 1px gutter on every side so the outside outline always has room.
        let (uw, uh) = ((vp.width - 2.0 * FIT_GUTTER).max(1.0), (vp.height - 2.0 * FIT_GUTTER).max(1.0));
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
    pub fn zoom_to_cursor(&mut self, factor: f32, cursor: (f32, f32), image: (u32, u32), vp: &Viewport) {
        let old = self.zoom;
        let new = (old * factor).clamp(MIN_ZOOM, MAX_ZOOM);
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
        self.pan = (self.pan.0.clamp(-lim_x, lim_x), self.pan.1.clamp(-lim_y, lim_y));
    }

    /// Map a surface-pixel position to image pixel coordinates (origin top-left). Inverse
    /// of [`Self::image_to_screen`]; the eyedropper (Phase 4) reads pixels through this.
    #[allow(dead_code)] // wired to the pixel inspector in Phase 4; unit-tested now
    pub fn screen_to_image(&self, screen: (f32, f32), image: (u32, u32), vp: &Viewport) -> (f32, f32) {
        let c = vp.center();
        let img_center = (c.0 + self.pan.0, c.1 + self.pan.1);
        let off = (screen.0 - img_center.0, screen.1 - img_center.1);
        (
            image.0 as f32 * 0.5 + off.0 / self.zoom,
            image.1 as f32 * 0.5 + off.1 / self.zoom,
        )
    }

    /// Map image pixel coordinates back to a surface-pixel position.
    #[allow(dead_code)] // pair of screen_to_image; exercised by the round-trip test
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
        assert!((before.0 - after.0).abs() < 0.5, "x: {} vs {}", before.0, after.0);
        assert!((before.1 - after.1).abs() < 0.5, "y: {} vs {}", before.1, after.1);
        assert!(!s.fit);
    }

    #[test]
    fn screen_image_round_trip() {
        let v = vp();
        let image = (1234u32, 567u32);
        let mut s = ViewState { zoom: 1.7, pan: (-30.0, 45.0), fit: false, fit_upscale: false };
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
        let mut s = ViewState { zoom: 1.0, pan: (0.0, 0.0), fit: false, fit_upscale: false };
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
