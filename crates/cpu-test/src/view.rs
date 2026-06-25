//! Pan/zoom/fit view math, copied verbatim (minus the GPU-uniform bits) from the daemon's
//! `render::view` so the CPU prototype navigates identically to the real viewer. Throwaway.

pub const MIN_ZOOM: f32 = 0.02;
pub const MAX_ZOOM: f32 = 64.0;

#[derive(Clone, Copy, Debug)]
pub struct Viewport {
    pub width: f32,
    pub height: f32,
    pub top_inset: f32,
    pub bottom_inset: f32,
}

impl Viewport {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width: width.max(1) as f32,
            height: height.max(1) as f32,
            top_inset: 0.0,
            bottom_inset: 0.0,
        }
    }

    pub fn usable(&self) -> (f32, f32) {
        let h = (self.height - self.top_inset - self.bottom_inset).max(1.0);
        (self.width.max(1.0), h)
    }

    pub fn center(&self) -> (f32, f32) {
        let (_, uh) = self.usable();
        (self.width * 0.5, self.top_inset + uh * 0.5)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    Rgb,
    R,
    G,
    B,
    A,
}

#[derive(Clone, Copy, Debug)]
pub struct ViewState {
    pub zoom: f32,
    pub pan: (f32, f32),
    pub fit: bool,
}

impl Default for ViewState {
    fn default() -> Self {
        Self { zoom: 1.0, pan: (0.0, 0.0), fit: true }
    }
}

impl ViewState {
    pub fn fit_to_window(&mut self, image: (u32, u32), vp: &Viewport) {
        let (uw, uh) = vp.usable();
        let (iw, ih) = (image.0.max(1) as f32, image.1.max(1) as f32);
        let z = (uw / iw).min(uh / ih).min(1.0);
        self.zoom = z.clamp(MIN_ZOOM, MAX_ZOOM);
        self.pan = (0.0, 0.0);
        self.fit = true;
    }

    pub fn one_to_one(&mut self) {
        self.zoom = 1.0;
        self.pan = (0.0, 0.0);
        self.fit = false;
    }

    pub fn zoom_to_cursor(&mut self, factor: f32, cursor: (f32, f32), image: (u32, u32), vp: &Viewport) {
        let old = self.zoom;
        let new = (old * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        if new == old {
            return;
        }
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

    pub fn zoom_centered(&mut self, factor: f32, image: (u32, u32), vp: &Viewport) {
        let c = vp.center();
        self.zoom_to_cursor(factor, c, image, vp);
    }

    pub fn pan_by(&mut self, delta: (f32, f32), image: (u32, u32), vp: &Viewport) {
        self.pan = (self.pan.0 + delta.0, self.pan.1 + delta.1);
        self.fit = false;
        self.clamp_pan(image, vp);
    }

    pub fn image_screen_size(&self, image: (u32, u32)) -> (f32, f32) {
        (image.0 as f32 * self.zoom, image.1 as f32 * self.zoom)
    }

    pub fn clamp_pan(&mut self, image: (u32, u32), vp: &Viewport) {
        let (uw, uh) = vp.usable();
        let (sw, sh) = self.image_screen_size(image);
        let lim_x = (sw - uw).abs() * 0.5;
        let lim_y = (sh - uh).abs() * 0.5;
        self.pan = (self.pan.0.clamp(-lim_x, lim_x), self.pan.1.clamp(-lim_y, lim_y));
    }
}
