//! Per-window CPU render state: a softbuffer surface over a raw Win32 child window plus the
//! pan/zoom/fit + channel/exposure/tonemap state driven by input. The threaded pixel work
//! lives in [`crate::render::shade`].
//!
//! softbuffer presents a packed `0x00RRGGBB` framebuffer to the window via GDI — no GPU
//! device, no D3D runtime. The decoded image is retained in `current_image` both as the
//! sampling source for shading and for the pixel inspector (#16).

use std::num::{NonZeroIsize, NonZeroU32};
use std::rc::Rc;

use fire_decode::DecodedImage;
use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawDisplayHandle,
    RawWindowHandle, Win32WindowHandle, WindowHandle, WindowsDisplayHandle,
};

use crate::render::shade::{self, Luts};
use crate::render::view::{Channel, DisplayState, Tonemap, ViewState, Viewport};

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::Graphics::Gdi::InvalidateRect;

/// A bare Win32 window handle (HWND + HINSTANCE) that softbuffer can target without winit.
struct WinHandle {
    hwnd: isize,
    hinstance: isize,
}

impl HasWindowHandle for WinHandle {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        let mut h = Win32WindowHandle::new(NonZeroIsize::new(self.hwnd).ok_or(HandleError::Unavailable)?);
        h.hinstance = NonZeroIsize::new(self.hinstance);
        // SAFETY: the HWND is a live window owned by this process for the surface's lifetime.
        Ok(unsafe { WindowHandle::borrow_raw(RawWindowHandle::Win32(h)) })
    }
}

impl HasDisplayHandle for WinHandle {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        // SAFETY: the Windows display handle carries no state.
        Ok(unsafe { DisplayHandle::borrow_raw(RawDisplayHandle::Windows(WindowsDisplayHandle::new())) })
    }
}

type Ctx = softbuffer::Context<Rc<WinHandle>>;
type Surf = softbuffer::Surface<Rc<WinHandle>, Rc<WinHandle>>;

pub struct SurfaceState {
    handle: Rc<WinHandle>,
    _context: Ctx,
    surface: Surf,
    luts: Luts,
    /// Packed letterbox / no-image backdrop color; overwritten with the theme color via
    /// [`Self::set_clear`] once the chrome is built.
    clear: u32,

    /// Monotonic per-window decode generation; a `DecodeDone` older than this is stale.
    generation: u64,
    /// The displayed image — sampling source for shading and the inspector (#16).
    current_image: Option<DecodedImage>,

    viewport: Viewport,
    view: ViewState,
    display: DisplayState,
    /// Last cursor position (surface px) — the anchor for wheel zoom-to-cursor.
    cursor: (f32, f32),
    /// Left button held → drag-pan in progress.
    dragging: bool,
}

impl SurfaceState {
    /// Build a softbuffer surface over a child window's HWND. `width`/`height` are the
    /// child's client size in physical px.
    pub fn new(hwnd: isize, hinstance: isize, width: u32, height: u32) -> Self {
        let handle = Rc::new(WinHandle { hwnd, hinstance });
        let context = softbuffer::Context::new(handle.clone()).expect("softbuffer context");
        let surface = softbuffer::Surface::new(&context, handle.clone()).expect("softbuffer surface");
        let luts = Luts::new();
        // Neutral near-black placeholder (linear 0.05, 0.05, 0.06), sRGB-encoded; only shown
        // until the win shell calls set_clear with the theme-aware backdrop.
        let enc = |lin: f32| {
            let i = (lin * 4096.0 + 0.5) as usize;
            luts.srgb[i.min(4096)] as u32
        };
        let clear = (enc(0.05) << 16) | (enc(0.05) << 8) | enc(0.06);

        Self {
            handle,
            _context: context,
            surface,
            luts,
            clear,
            generation: 0,
            current_image: None,
            viewport: Viewport::new(width, height),
            view: ViewState::default(),
            display: DisplayState::default(),
            cursor: (0.0, 0.0),
            dragging: false,
        }
    }

    /// Set the letterbox / no-image backdrop color (packed `0x00RRGGBB`), so it tracks the
    /// light/dark theme of the chrome instead of a fixed near-black.
    pub fn set_clear(&mut self, packed: u32) {
        self.clear = packed;
    }

    pub fn next_generation(&mut self) -> u64 {
        self.generation += 1;
        self.generation
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// CPU pixels of the displayed image, retained for the pixel inspector (#16).
    pub fn current_image(&self) -> Option<&DecodedImage> {
        self.current_image.as_ref()
    }

    // --- read-only view of display state, for the chrome (toolbar button states + status bar) ---

    /// Current zoom as a whole percentage (100 = 1:1), for the status bar.
    pub fn zoom_percent(&self) -> u32 {
        (self.view.zoom * 100.0).round().max(0.0) as u32
    }

    /// Active channel-isolation mode (drives the R/G/B/A/RGB toolbar toggles).
    pub fn channel(&self) -> Channel {
        self.display.channel
    }

    /// Active HDR tonemap operator (drives the ACES toggle).
    pub fn tonemap(&self) -> Tonemap {
        self.display.tonemap
    }

    /// Whether the view is in fit mode (drives the Fit toggle).
    pub fn is_fit(&self) -> bool {
        self.view.fit
    }

    /// Current exposure in stops (HDR only), for the status bar.
    pub fn exposure(&self) -> f32 {
        self.display.exposure
    }

    /// Whether the displayed image is HDR/linear (gates the ACES + exposure controls).
    pub fn is_hdr(&self) -> bool {
        self.current_image.as_ref().is_some_and(|i| i.format.is_hdr())
    }

    fn image_dims(&self) -> Option<(u32, u32)> {
        self.current_image.as_ref().map(|i| (i.width, i.height))
    }

    /// Drop the displayed image so the next paint shows the placeholder (avoids flashing the
    /// previous file's pixels while a freshly-opened file decodes).
    pub fn clear_image(&mut self) {
        self.current_image = None;
    }

    /// Adopt a decoded image: retain it, reset to fit + neutral display state for the new
    /// file (#17). No GPU upload — shading samples `current_image` directly.
    pub fn set_image(&mut self, img: DecodedImage) {
        let (w, h) = (img.width, img.height);
        self.current_image = Some(img);
        self.display = DisplayState::default();
        self.view.fit_to_window((w, h), &self.viewport);
    }

    /// Resize the view to a new client size (physical px); re-fit or clamp the pan.
    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.viewport = Viewport::new(width, height);
        if let Some(dims) = self.image_dims() {
            if self.view.fit {
                self.view.fit_to_window(dims, &self.viewport);
            } else {
                self.view.clamp_pan(dims, &self.viewport);
            }
        }
        self.request_redraw();
    }

    /// Schedule a repaint of the window (delivered as `WM_PAINT`).
    pub fn invalidate(&self) {
        // SAFETY: hwnd is a live window; null rect invalidates the whole client area.
        unsafe { InvalidateRect(self.handle.hwnd as HWND, std::ptr::null(), 0) };
    }

    fn request_redraw(&self) {
        self.invalidate();
    }

    /// Shade the current image into the softbuffer framebuffer and present. Called from the
    /// view child's `WM_PAINT`.
    pub fn render(&mut self) {
        let w = self.viewport.width as u32;
        let h = self.viewport.height as u32;
        if w == 0 || h == 0 {
            return;
        }
        if self
            .surface
            .resize(NonZeroU32::new(w).unwrap(), NonZeroU32::new(h).unwrap())
            .is_err()
        {
            return;
        }
        let mut buf = match self.surface.buffer_mut() {
            Ok(b) => b,
            Err(_) => return,
        };
        match &self.current_image {
            Some(img) => shade::shade(
                &mut buf,
                w,
                h,
                img,
                &self.view,
                &self.display,
                &self.viewport,
                &self.luts,
                self.clear,
            ),
            None => buf.fill(self.clear),
        }
        let _ = buf.present();
    }

    fn refresh(&self) {
        self.request_redraw();
    }

    // --- input-driven view controls (called from the win shell) ----------------

    pub fn on_cursor_moved(&mut self, pos: (f32, f32)) {
        let delta = (pos.0 - self.cursor.0, pos.1 - self.cursor.1);
        self.cursor = pos;
        if self.dragging {
            if let Some(dims) = self.image_dims() {
                self.view.pan_by(delta, dims, &self.viewport);
                self.refresh();
            }
        }
    }

    pub fn begin_drag(&mut self) {
        self.dragging = true;
    }

    pub fn end_drag(&mut self) {
        self.dragging = false;
    }

    pub fn zoom_at_cursor(&mut self, factor: f32) {
        if let Some(dims) = self.image_dims() {
            self.view.zoom_to_cursor(factor, self.cursor, dims, &self.viewport);
            self.refresh();
        }
    }

    pub fn zoom_centered(&mut self, factor: f32) {
        if let Some(dims) = self.image_dims() {
            self.view.zoom_centered(factor, dims, &self.viewport);
            self.refresh();
        }
    }

    pub fn fit(&mut self) {
        if let Some(dims) = self.image_dims() {
            self.view.fit_to_window(dims, &self.viewport);
            self.refresh();
        }
    }

    pub fn one_to_one(&mut self) {
        self.view.one_to_one();
        if let Some(dims) = self.image_dims() {
            self.view.clamp_pan(dims, &self.viewport);
        }
        self.refresh();
    }

    pub fn toggle_channel(&mut self, ch: Channel) {
        self.display.channel = if self.display.channel == ch { Channel::Rgb } else { ch };
        self.refresh();
    }

    pub fn set_channel(&mut self, ch: Channel) {
        self.display.channel = ch;
        self.refresh();
    }

    pub fn adjust_exposure(&mut self, delta: f32) {
        self.display.exposure = (self.display.exposure + delta).clamp(-16.0, 16.0);
        self.refresh();
    }

    pub fn toggle_tonemap(&mut self) {
        self.display.tonemap = match self.display.tonemap {
            Tonemap::Reinhard => Tonemap::Aces,
            Tonemap::Aces => Tonemap::Reinhard,
        };
        self.refresh();
    }
}
