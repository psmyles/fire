//! GPU viewport: a Direct3D 11 renderer that presents the decoded image through a DXGI
//! flip-model swapchain on the child "view" window. The image lives as a GPU texture (with a
//! hardware mip chain), and pan/zoom/exposure/channel/tonemap are just constant-buffer
//! values, so each frame is one textured fullscreen triangle: the per-frame CPU cost is ~an
//! 80-byte upload + a draw call, and the GPU does the sampling and the whole color pipeline.
//!
//! Panning changes a transform and the GPU re-samples the texture rather than re-running a
//! per-pixel pipeline on the CPU. Presentation is vsync-paced through the flip-model swapchain
//! (tear-free at high refresh), so interaction stays smooth while the CPU sits near idle.
//!
//! Color: 8-bit sources upload as `*_UNORM_SRGB` (hardware sRGB→linear on sample), float
//! sources are already linear, 16-bit unorm is sRGB-decoded in the shader. The pixel shader
//! outputs **linear** and the render-target view is `*_SRGB`, so the hardware sRGB-encodes on
//! write. The whole pipeline is linear.

use std::ffi::c_void;
use std::sync::Arc;

use fire_decode::{AnimationFrame, DecodedImage, PixelFormat};

use windows::core::Interface;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
    D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Buffer, ID3D11Device, ID3D11DeviceContext, ID3D11PixelShader,
    ID3D11RenderTargetView, ID3D11SamplerState, ID3D11ShaderResourceView, ID3D11Texture2D,
    ID3D11VertexShader, D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_BUFFER_DESC, D3D11_COMPARISON_NEVER, D3D11_CPU_ACCESS_WRITE,
    D3D11_CREATE_DEVICE_FLAG, D3D11_FILTER_ANISOTROPIC, D3D11_FILTER_MIN_MAG_MIP_POINT,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_WRITE_DISCARD, D3D11_RENDER_TARGET_VIEW_DESC,
    D3D11_RENDER_TARGET_VIEW_DESC_0, D3D11_RESOURCE_MISC_GENERATE_MIPS,
    D3D11_RTV_DIMENSION_TEXTURE2D, D3D11_SAMPLER_DESC, D3D11_SDK_VERSION, D3D11_TEX2D_RTV,
    D3D11_TEXTURE2D_DESC, D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_USAGE_DEFAULT, D3D11_USAGE_DYNAMIC,
    D3D11_VIEWPORT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_R16G16B16A16_UNORM,
    DXGI_FORMAT_R32G32B32A32_FLOAT, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM_SRGB,
    DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIAdapter, IDXGIDevice, IDXGIFactory2, IDXGISwapChain1, DXGI_PRESENT, DXGI_SCALING_STRETCH,
    DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_EFFECT_FLIP_DISCARD,
    DXGI_USAGE_RENDER_TARGET_OUTPUT,
};

use windows_sys::Win32::Foundation::HWND as SysHwnd;
use windows_sys::Win32::Graphics::Gdi::InvalidateRect;

use crate::render::view::{Background, Channel, DisplayState, Tonemap, ViewState, Viewport};

/// Scrubby-zoom sensitivity: an RMB vertical drag multiplies zoom by `exp(dy * this)` per pixel
/// (~2.7× per 100 px). Exponential-in-pixels so the gesture feels uniform across the zoom range;
/// drag down (dy > 0) zooms in, up zooms out.
const ZOOM_DRAG_SENSITIVITY: f32 = 0.01;

/// How far (surface px) the cursor may move during an RMB press before it counts as a zoom-drag
/// rather than a right-*click*. Below this the gesture opens the context menu; the tiny zoom such a
/// jiggle would apply (~exp(slop·sensitivity), a couple percent) is imperceptible.
const ZOOM_DRAG_CLICK_SLOP: f32 = 5.0;

/// Per-frame shader constants. Layout matches the HLSL `cbuffer` (16-byte float4 registers);
/// keep the field order/padding in lockstep with the `Params` cbuffer in `render/shader.hlsl`.
/// 112 bytes = 7 float4 registers.
#[repr(C)]
#[derive(Clone, Copy)]
struct Params {
    img_w: f32,
    img_h: f32,
    surf_w: f32,
    surf_h: f32,
    pan_x: f32,
    pan_y: f32,
    inv_zoom: f32,
    exposure: f32,
    channel: i32,
    tonemap: i32,
    is_hdr: i32,
    has_image: i32,
    linear_sample: i32,
    /// Viewport backdrop mode (0=black 1=white 2=grey 3=checker); see [`Background`].
    background: i32,
    /// 1 → draw a 1px outline around the image boundary.
    outline: i32,
    /// 1 → flipbook mode: `img_w/img_h` are the (fractional) frame rect, and the fields below
    /// select the cell(s) to sample from the sheet. 0 → whole-image sampling (fields ignored).
    fb_on: i32,
    clear_r: f32,
    clear_g: f32,
    clear_b: f32,
    clear_a: f32,
    // Flipbook cell selection (all in sheet texels). Identity when `fb_on == 0`.
    sheet_w: f32,
    sheet_h: f32,
    cell_a_x: f32,
    cell_a_y: f32,
    cell_b_x: f32,
    cell_b_y: f32,
    /// Crossfade factor toward cell B (0 = hard cut).
    fb_blend: f32,
    /// Mip-LOD clamp so minified samples can't bleed across cell boundaries (`f32::MAX` = none).
    fb_max_lod: f32,
}

// The cbuffer layout is kept in lockstep with the HLSL `cbuffer Params` in `render/shader.hlsl`
// by hand (there is no reflection); these guard the size so a field added on only one side is a
// build error rather than silent visual corruption. 112 bytes = 7 × 16-byte float4 registers.
const _: () = assert!(std::mem::size_of::<Params>() == 112);
const _: () = assert!(std::mem::size_of::<Params>().is_multiple_of(16));

/// Flipbook render parameters mirrored onto the surface from the active per-path state (the
/// surface never owns durable flipbook state — the win shell does). `None` = not in flipbook mode.
#[derive(Debug, Clone, Copy)]
pub struct FlipbookParams {
    pub grid: crate::flipbook::Grid,
    pub frame_count: u32,
    pub frame_pos: f32,
    pub blend: bool,
}

/// GPU render state for the view window: the D3D11 device/swapchain plus the same pan/zoom/fit
/// and channel/exposure/tonemap state the CPU surface carried (so the window shell and chrome
/// drive it through an identical API).
pub struct GpuSurface {
    hwnd: isize,

    device: ID3D11Device,
    context: ID3D11DeviceContext,
    swapchain: IDXGISwapChain1,
    /// Recreated lazily after a resize (the backbuffer changes).
    rtv: Option<ID3D11RenderTargetView>,

    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    samp_aniso: ID3D11SamplerState,
    samp_point: ID3D11SamplerState,
    cbuffer: ID3D11Buffer,

    /// Current image texture + its sampling view (None until the first image lands).
    _tex: Option<ID3D11Texture2D>,
    srv: Option<ID3D11ShaderResourceView>,
    /// 1 if the texture samples already-linear (8-bit `*_SRGB` / float), 0 if the shader must
    /// sRGB-decode (16-bit unorm).
    linear_sample: i32,

    /// No-image backdrop (empty window), packed sRGB and its linear form (for the `*_SRGB` RTV).
    /// Once an image is loaded the [`Background`] mode owns the viewport instead.
    clear: u32,
    clear_lin: [f32; 4],

    /// Viewport backdrop while an image is shown; defaults per-image (opaque → black, alpha →
    /// checker) and is overridden by the toolbar's background buttons.
    background: Background,
    /// The user's explicit backdrop pick, if any. Once set via the toolbar it sticks for the rest
    /// of the session (every later image adopts it instead of its per-type default); `None` until
    /// the user chooses, so each image still gets its natural default before the first override.
    background_override: Option<Background>,

    /// Draw a 1px outline around the image boundary (toolbar toggle). On by default for every
    /// image type; the pick persists across navigation, like the backdrop.
    outline: bool,

    /// Monotonic per-window decode generation; a `DecodeDone` older than this is stale.
    generation: u64,
    /// The displayed image — retained for the pixel inspector (#16) and for re-fit on resize.
    /// For an animated source this holds frame 0 (dimensions/format/metadata are frame-invariant);
    /// the frames themselves live in `anim_frames`. Held behind an `Arc` because the decode worker
    /// keeps a clone alive to run flipbook detection *after* the image has been posted for display
    /// (so detection never delays time-to-first-pixel); both sides only ever read it.
    current_image: Option<Arc<DecodedImage>>,

    /// Frames of an animated image (animated GIF), each a full RGBA canvas + delay. Empty for a
    /// still image. `anim_index` is the frame currently uploaded to the texture; the playback
    /// timer (owned by the win shell) advances it via [`Self::advance_frame`].
    anim_frames: Vec<AnimationFrame>,
    anim_index: usize,

    viewport: Viewport,
    view: ViewState,
    /// Active flipbook render parameters, mirrored from the win shell's per-path state. `Some`
    /// makes pan/zoom/fit operate on the frame rect and the shader sample a single cell. The
    /// surface never persists this — it is (re)applied on every adopt via [`Self::set_flipbook`].
    flipbook: Option<FlipbookParams>,
    /// Whether the explicit "fit to window" command (`F` / toolbar) scales *small* images up to
    /// fill the surface (the `fit-upscale` config key). False keeps the texture-viewer cap at 1:1.
    /// This governs only the explicit command — how an image *opens* is `open_actual_size`.
    fit_upscale: bool,
    /// Whether a freshly adopted image opens at native 1:1 instead of fitted (the `default-fit`
    /// config key). Fitted-on-open (the default) never upscales, so a small image shows at 100%
    /// either way; this only changes what an *oversized* image does on open.
    open_actual_size: bool,
    /// The tonemap operator a freshly adopted image starts on (the `default-tonemap` config key).
    /// Seeds [`DisplayState`] on each adopt; the `T` toggle still moves the live one.
    default_tonemap: Tonemap,
    display: DisplayState,
    cursor: (f32, f32),
    dragging: bool,
    /// RMB scrubby-zoom: whether a zoom-drag is active, the pivot (the press point, surface px),
    /// and the last cursor-y so each move applies an incremental zoom.
    zoom_dragging: bool,
    zoom_anchor: (f32, f32),
    zoom_last_y: f32,
    /// Whether the active RMB gesture has moved past [`ZOOM_DRAG_CLICK_SLOP`]; if not, the release
    /// is treated as a right-click (opens the context menu) rather than the end of a zoom-drag.
    zoom_dragged: bool,
}

impl GpuSurface {
    /// Build the D3D11 device + flip-model swapchain on the child view HWND. `_hinstance` is
    /// unused (D3D needs only the HWND); kept for signature parity with the CPU surface.
    pub fn new(hwnd: isize, _hinstance: isize, width: u32, height: u32, fit_upscale: bool) -> Self {
        let (device, context) = create_device();
        let win_hwnd = HWND(hwnd as *mut c_void);
        let swapchain = create_swapchain(&device, win_hwnd, width.max(1), height.max(1));

        let (vs, ps) = create_shaders(&device);
        let (samp_aniso, samp_point) = create_samplers(&device);
        let cbuffer = create_const_buffer(&device);

        Self {
            hwnd,
            device,
            context,
            swapchain,
            rtv: None,
            vs,
            ps,
            samp_aniso,
            samp_point,
            cbuffer,
            _tex: None,
            srv: None,
            linear_sample: 1,
            clear: 0,
            clear_lin: [0.0, 0.0, 0.0, 1.0],
            background: Background::Black,
            background_override: None,
            outline: true,
            generation: 0,
            current_image: None,
            anim_frames: Vec::new(),
            anim_index: 0,
            viewport: Viewport::new(width, height),
            view: ViewState::default(),
            flipbook: None,
            fit_upscale,
            open_actual_size: false,
            default_tonemap: Tonemap::Reinhard,
            display: DisplayState::default(),
            cursor: (0.0, 0.0),
            dragging: false,
            zoom_dragging: false,
            zoom_anchor: (0.0, 0.0),
            zoom_last_y: 0.0,
            zoom_dragged: false,
        }
    }

    /// Set the letterbox / no-image backdrop color (packed `0x00RRGGBB`); stored both packed and
    /// as linear floats so the `*_SRGB` render target re-encodes it to the intended sRGB.
    pub fn set_clear(&mut self, packed: u32) {
        self.clear = packed;
        let dec = |b: u32| srgb_to_linear((b & 0xff) as f32 / 255.0);
        self.clear_lin = [dec(packed >> 16), dec(packed >> 8), dec(packed), 1.0];
    }

    pub fn next_generation(&mut self) -> u64 {
        self.generation += 1;
        self.generation
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn current_image(&self) -> Option<&DecodedImage> {
        self.current_image.as_deref()
    }

    // --- read-only view of display state, for the chrome ---

    pub fn zoom_percent(&self) -> u32 {
        (self.view.zoom * 100.0).round().max(0.0) as u32
    }

    pub fn channel(&self) -> Channel {
        self.display.channel
    }

    pub fn tonemap(&self) -> Tonemap {
        self.display.tonemap
    }

    pub fn is_fit(&self) -> bool {
        self.view.fit
    }

    pub fn exposure(&self) -> f32 {
        self.display.exposure
    }

    pub fn is_hdr(&self) -> bool {
        self.current_image
            .as_ref()
            .is_some_and(|i| i.format.is_hdr())
    }

    /// Whether the current image carries an alpha channel (gray+A or RGBA source) — drives the
    /// RGB↔RGBA toolbar icon and keeps the alpha-channel isolation control available. This is true
    /// even when the alpha is entirely opaque, so the user can always inspect it; whether that
    /// alpha actually holds transparency (and thus defaults to the checker backdrop) is a separate
    /// signal handled in [`Self::set_image`] via `DecodedImage::alpha_opaque`.
    pub fn has_alpha(&self) -> bool {
        self.current_image
            .as_ref()
            .is_some_and(|i| matches!(i.channels, 2 | 4))
    }

    pub fn background(&self) -> Background {
        self.background
    }

    /// Set the viewport backdrop (toolbar override) and repaint. Records the pick so it persists
    /// across image navigation for the rest of the session (see [`Self::background_override`]).
    pub fn set_background(&mut self, bg: Background) {
        self.background = bg;
        self.background_override = Some(bg);
        self.refresh();
    }

    /// Apply the configured backdrop preference (settings dialog / startup): `Some` pins that
    /// backdrop for every image, `None` restores the per-image default (checker for real
    /// transparency, black otherwise) — including for the image already on screen.
    pub fn set_background_pref(&mut self, bg: Option<Background>) {
        self.background_override = bg;
        self.background = bg.unwrap_or_else(|| {
            self.current_image
                .as_ref()
                .map_or(Background::Black, |img| default_background(img))
        });
        self.refresh();
    }

    /// Whether the explicit fit command upscales small images (the `fit-upscale` config key).
    pub fn set_fit_upscale(&mut self, on: bool) {
        self.fit_upscale = on;
    }

    /// Whether a newly opened image lands at native 1:1 rather than fitted (`default-fit`). Takes
    /// effect on the next adopt — it never yanks the view of the image already on screen.
    pub fn set_open_actual_size(&mut self, on: bool) {
        self.open_actual_size = on;
    }

    /// The tonemap a newly adopted image starts on (`default-tonemap`). Like `set_open_actual_size`,
    /// this seeds the *next* image: the current one keeps whatever the user toggled it to.
    pub fn set_default_tonemap(&mut self, t: Tonemap) {
        self.default_tonemap = t;
    }

    pub fn outline(&self) -> bool {
        self.outline
    }

    /// Toggle the image-boundary outline and repaint.
    pub fn toggle_outline(&mut self) {
        self.outline = !self.outline;
        self.refresh();
    }

    fn image_dims(&self) -> Option<(u32, u32)> {
        self.current_image.as_ref().map(|i| (i.width, i.height))
    }

    /// Dimensions the pan/zoom/fit math operates on: the frame rect in flipbook mode, else the
    /// whole image. All view-control call sites use this so entering the mode or changing the
    /// grid re-fits and clamps against the frame.
    fn view_dims(&self) -> Option<(u32, u32)> {
        let (w, h) = self.image_dims()?;
        Some(match self.flipbook {
            Some(fb) => crate::flipbook::frame_dims(fb.grid, (w, h)),
            None => (w, h),
        })
    }

    /// Adopt (or clear) flipbook render parameters. Re-fits the view to the frame rect only when
    /// entering/leaving the mode or when the grid changes (so playback/scrub position changes,
    /// which call [`Self::set_flipbook_pos`], don't disturb the user's pan/zoom). Repaints.
    pub fn set_flipbook(&mut self, fb: Option<FlipbookParams>) {
        let old_grid = self.flipbook.map(|f| f.grid);
        let new_grid = fb.map(|f| f.grid);
        self.flipbook = fb;
        if old_grid != new_grid {
            // Entering/leaving the mode or a grid edit changes the fitted content size → re-fit
            // without upscaling (same rule as opening an image), against the new view dims.
            if let Some(dims) = self.view_dims() {
                self.view.fit_to_window(dims, &self.viewport, false);
            }
        }
        self.refresh();
    }

    /// Update only the fractional playback position (the hot path: playback tick / slider scrub).
    /// No re-fit; just repaint.
    pub fn set_flipbook_pos(&mut self, frame_pos: f32) {
        if let Some(fb) = &mut self.flipbook {
            fb.frame_pos = frame_pos;
            self.refresh();
        }
    }

    /// Drop the displayed image so the next paint shows the placeholder. Also drops any animation
    /// frames so the win shell's next `frame_delay_ms()` returns `None` and the playback timer stops.
    pub fn clear_image(&mut self) {
        self.current_image = None;
        self._tex = None;
        self.srv = None;
        self.anim_frames.clear();
        self.anim_index = 0;
        self.flipbook = None;
    }

    /// Adopt a decoded image: upload it as a GPU texture (hardware mip chain) and reset to fit +
    /// neutral display state for the new file (#17). Returns the GPU error if the upload fails
    /// (e.g. `E_OUTOFMEMORY` on a very large image) so the caller can report it instead of the
    /// process aborting; on failure the prior display state is left untouched.
    pub fn set_image(&mut self, img: Arc<DecodedImage>) -> windows::core::Result<()> {
        let (w, h) = (img.width, img.height);
        // Upload first: if the GPU rejects the texture we bail here, before mutating any state,
        // so a failed adopt can't leave the surface half-updated.
        self.upload_texture(&img)?;
        // Adopt any animation frames for playback and start from frame 0 (already uploaded above).
        // The image is shared with the decode worker (for flipbook detection), so frames are cloned
        // rather than moved out; a still image (the shared case) leaves the list empty and clones
        // nothing. The win shell arms the timer from `frame_delay_ms()` after this.
        self.anim_frames = img
            .animation
            .as_ref()
            .map(|a| a.frames.clone())
            .unwrap_or_default();
        self.anim_index = 0;
        // Pick the viewport backdrop: an explicit pick (the toolbar's background buttons, or the
        // `background` config key) sticks across every image; otherwise default to the image's
        // nature — see `default_background`.
        self.background = self
            .background_override
            .unwrap_or_else(|| default_background(&img));
        self.current_image = Some(img);
        // Neutral display state for the new file (#17), seeded with the configured tonemap.
        self.display = DisplayState {
            tonemap: self.default_tonemap,
            ..DisplayState::default()
        };
        // A fresh image starts as a whole-image view; the win shell re-applies any per-path
        // flipbook state (via `set_flipbook`) right after this adopt, which re-fits to the frame.
        self.flipbook = None;
        // Every newly opened image (including folder ←/→ navigation) fits *without* upscaling: a
        // large image shrinks to fit, a small one shows at native 1:1. The explicit fit command
        // (`F` / toolbar) can still fill the surface — see `fit_upscale` / `fit`. With
        // `default-fit = "actual-size"` an image instead opens at 100% however large it is.
        if self.open_actual_size {
            self.view.one_to_one();
        } else {
            self.view.fit_to_window((w, h), &self.viewport, false);
        }
        Ok(())
    }

    /// Adopt a hot-reloaded image *without* resetting the view: upload the new pixels and keep the
    /// current pan / zoom / channel / exposure / tonemap. Used when the file changed on disk and
    /// the re-decode came back at the same dimensions (the "re-export same canvas" case), so the
    /// user's zoomed-in detail and display state survive the update. The pan is re-clamped
    /// defensively (a no-op while the dims are unchanged).
    pub fn replace_image_keep_view(&mut self, img: Arc<DecodedImage>) -> windows::core::Result<()> {
        self.upload_texture(&img)?;
        // Refresh the animation frames from the re-decoded file and restart from frame 0 (the view
        // is preserved, but the animation plays from the top). The win shell re-arms the timer.
        self.anim_frames = img
            .animation
            .as_ref()
            .map(|a| a.frames.clone())
            .unwrap_or_default();
        self.anim_index = 0;
        self.current_image = Some(img);
        // Hot reload keeps flipbook mode active (same path); clamp against the frame rect when in
        // flipbook mode, else the whole image (a no-op while the dims are unchanged).
        if !self.view.fit {
            if let Some(vd) = self.view_dims() {
                self.view.clamp_pan(vd, &self.viewport);
            }
        }
        Ok(())
    }

    /// Delay (ms) the currently displayed animation frame should be shown before advancing, or
    /// `None` for a still image. The win shell arms the playback timer from this after every adopt.
    pub fn frame_delay_ms(&self) -> Option<u32> {
        (self.anim_frames.len() > 1).then(|| self.anim_frames[self.anim_index].delay_ms)
    }

    /// Advance to the next animation frame (wrapping) and upload it as the texture, returning the
    /// now-current frame's delay (ms) so the caller can reschedule the timer (GIF frame delays
    /// vary). Returns `None` and does nothing for a still image. On a GPU upload error the visible
    /// frame is left unchanged and the current frame's delay is returned, so a transient failure
    /// paces the retry rather than wedging playback.
    pub fn advance_frame(&mut self) -> Option<u32> {
        let n = self.anim_frames.len();
        if n <= 1 {
            return None;
        }
        let (w, h) = self.image_dims()?;
        let format = self.current_image.as_ref()?.format;
        let next = (self.anim_index + 1) % n;
        match create_image_texture(
            &self.device,
            &self.context,
            &self.anim_frames[next].pixels,
            w,
            h,
            format,
        ) {
            Ok((tex, srv, linear)) => {
                self._tex = Some(tex);
                self.srv = Some(srv);
                self.linear_sample = linear;
                self.anim_index = next;
            }
            Err(e) => eprintln!("fire: animation frame upload failed: {e}"),
        }
        Some(self.anim_frames[self.anim_index].delay_ms)
    }

    /// Upload `img`'s (frame-0) pixels as a `DEFAULT` texture with a full mip chain generated on the
    /// GPU. Returns the GPU error rather than panicking if texture/SRV creation fails — this runs
    /// synchronously from the wndproc (via `decode_done`), where a panic would unwind across the
    /// Win32 boundary and abort the process.
    fn upload_texture(&mut self, img: &DecodedImage) -> windows::core::Result<()> {
        let (tex, srv, linear_sample) = create_image_texture(
            &self.device,
            &self.context,
            &img.pixels,
            img.width,
            img.height,
            img.format,
        )?;
        self._tex = Some(tex);
        self.srv = Some(srv);
        self.linear_sample = linear_sample;
        Ok(())
    }

    /// Resize the view to a new client size (physical px): resize the swapchain buffers, drop
    /// the stale RTV, and re-fit or clamp the pan.
    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.viewport = Viewport::new(width, height);
        self.rtv = None;
        unsafe {
            if let Err(e) = self.swapchain.ResizeBuffers(
                0,
                width,
                height,
                DXGI_FORMAT_UNKNOWN,
                DXGI_SWAP_CHAIN_FLAG(0),
            ) {
                // A failed resize leaves the backbuffer at its old size; log it (ensure_rtv will
                // recreate the RTV from whatever the swapchain reports next paint).
                eprintln!("fire: swapchain ResizeBuffers failed: {e}");
            }
        }
        if let Some(dims) = self.view_dims() {
            if self.view.fit {
                // Re-fit with the rule the active fit used (load = no upscale, explicit fit =
                // `fit_upscale`), so a resize doesn't silently switch a small image's scaling.
                self.view
                    .fit_to_window(dims, &self.viewport, self.view.fit_upscale);
            } else {
                self.view.clamp_pan(dims, &self.viewport);
            }
        }
        self.request_redraw();
    }

    /// Schedule a repaint (delivered as `WM_PAINT`).
    pub fn invalidate(&self) {
        // SAFETY: hwnd is a live window; null rect invalidates the whole client area.
        unsafe { InvalidateRect(self.hwnd as SysHwnd, std::ptr::null(), 0) };
    }

    fn request_redraw(&self) {
        self.invalidate();
    }

    fn refresh(&self) {
        self.request_redraw();
    }

    /// (Re)create the render-target view over the current backbuffer, as an `*_SRGB` view so the
    /// shader's linear output is sRGB-encoded on write. On failure (e.g. a device-removed / TDR
    /// reset) it leaves `rtv` as `None` and logs; `render` then skips drawing this frame rather
    /// than panicking inside the paint wndproc.
    fn ensure_rtv(&mut self) {
        if self.rtv.is_some() {
            return;
        }
        unsafe {
            let back: ID3D11Texture2D = match self.swapchain.GetBuffer(0) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("fire: swapchain GetBuffer failed: {e}");
                    return;
                }
            };
            let desc = D3D11_RENDER_TARGET_VIEW_DESC {
                Format: DXGI_FORMAT_R8G8B8A8_UNORM_SRGB,
                ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_RTV { MipSlice: 0 },
                },
            };
            let mut rtv: Option<ID3D11RenderTargetView> = None;
            if let Err(e) = self
                .device
                .CreateRenderTargetView(&back, Some(&desc), Some(&mut rtv))
            {
                eprintln!("fire: CreateRenderTargetView failed: {e}");
                return;
            }
            self.rtv = rtv;
        }
    }

    /// Draw the image (or the letterbox) and present. Called from the view child's `WM_PAINT`.
    /// One fullscreen triangle; the pixel shader does the sampling + color pipeline.
    pub fn render(&mut self) {
        let w = self.viewport.width as u32;
        let h = self.viewport.height as u32;
        if w == 0 || h == 0 {
            return;
        }
        self.ensure_rtv();
        let rtv = match &self.rtv {
            Some(r) => r.clone(),
            None => return,
        };

        let is_hdr = self.is_hdr();
        // Flipbook mode maps the surface into a single frame rect: `img_w/img_h` become the
        // (fractional) cell size and the fb_* fields pick which cell(s) of the sheet to sample.
        // Off (still image / whole sheet), the fb fields are identity so the shader path below is
        // untouched. Every field is set explicitly (no `..default()`), matching the checker note.
        let (img_w, img_h, has_image, fbf) = match self.current_image.as_ref().zip(self.flipbook) {
            Some((img, fbp)) => {
                let sheet = (img.width, img.height);
                let (fw, fh) = (
                    img.width as f32 / fbp.grid.cols.max(1) as f32,
                    img.height as f32 / fbp.grid.rows.max(1) as f32,
                );
                let (a, b, blend) =
                    crate::flipbook::resolve_frames(fbp.frame_pos, fbp.frame_count, fbp.blend);
                let (ax, ay) = crate::flipbook::frame_cell_offset(a, fbp.grid, sheet);
                let (bx, by) = crate::flipbook::frame_cell_offset(b, fbp.grid, sheet);
                let lod = crate::flipbook::max_lod(fbp.grid, sheet);
                (fw, fh, 1, Some((sheet, (ax, ay), (bx, by), blend, lod)))
            }
            None => match &self.current_image {
                Some(img) => (img.width as f32, img.height as f32, 1, None),
                None => (1.0, 1.0, 0, None),
            },
        };
        // Identity flipbook fields when off (fb_on == 0 → shader ignores them, but keep them sane).
        let (sheet_w, sheet_h, ca, cb, fb_blend, fb_max_lod, fb_on) = match fbf {
            Some((sheet, ca, cb, blend, lod)) => {
                (sheet.0 as f32, sheet.1 as f32, ca, cb, blend, lod, 1)
            }
            None => (img_w, img_h, (0.0, 0.0), (0.0, 0.0), 0.0, f32::MAX, 0),
        };
        let params = Params {
            img_w,
            img_h,
            surf_w: self.viewport.width,
            surf_h: self.viewport.height,
            pan_x: self.view.pan.0,
            pan_y: self.view.pan.1,
            inv_zoom: 1.0 / self.view.zoom,
            exposure: if is_hdr {
                self.display.exposure.exp2()
            } else {
                1.0
            },
            channel: channel_code(self.display.channel),
            tonemap: match self.display.tonemap {
                Tonemap::Reinhard => 0,
                Tonemap::Aces => 1,
            },
            is_hdr: is_hdr as i32,
            has_image,
            linear_sample: self.linear_sample,
            background: background_code(self.background),
            outline: self.outline as i32,
            fb_on,
            clear_r: self.clear_lin[0],
            clear_g: self.clear_lin[1],
            clear_b: self.clear_lin[2],
            clear_a: 1.0,
            sheet_w,
            sheet_h,
            cell_a_x: ca.0,
            cell_a_y: ca.1,
            cell_b_x: cb.0,
            cell_b_y: cb.1,
            fb_blend,
            fb_max_lod,
        };

        unsafe {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            if self
                .context
                .Map(
                    &self.cbuffer,
                    0,
                    D3D11_MAP_WRITE_DISCARD,
                    0,
                    Some(&mut mapped),
                )
                .is_ok()
            {
                std::ptr::copy_nonoverlapping(
                    &params as *const Params as *const u8,
                    mapped.pData as *mut u8,
                    std::mem::size_of::<Params>(),
                );
                self.context.Unmap(&self.cbuffer, 0);
            }

            let vp = D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: w as f32,
                Height: h as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            };
            self.context.RSSetViewports(Some(&[vp]));
            self.context
                .OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
            self.context
                .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            self.context.VSSetShader(&self.vs, None);
            self.context.PSSetShader(&self.ps, None);
            self.context
                .PSSetConstantBuffers(0, Some(&[Some(self.cbuffer.clone())]));
            self.context
                .PSSetShaderResources(0, Some(std::slice::from_ref(&self.srv)));
            self.context.PSSetSamplers(
                0,
                Some(&[Some(self.samp_aniso.clone()), Some(self.samp_point.clone())]),
            );
            self.context.Draw(3, 0);
            // Sync interval 1 → vsync-paced (tear-free); event-driven, so no idle frames.
            let _ = self.swapchain.Present(1, DXGI_PRESENT(0));
        }
    }

    // --- input-driven view controls (called from the win shell) ----------------

    pub fn on_cursor_moved(&mut self, pos: (f32, f32)) {
        let delta = (pos.0 - self.cursor.0, pos.1 - self.cursor.1);
        self.cursor = pos;
        if self.dragging {
            if let Some(dims) = self.view_dims() {
                self.view.pan_by(delta, dims, &self.viewport);
                self.refresh();
            }
        } else if self.zoom_dragging {
            // Past the click slop this is a real zoom-drag, so the release won't open the menu.
            if !self.zoom_dragged {
                let (ax, ay) = (pos.0 - self.zoom_anchor.0, pos.1 - self.zoom_anchor.1);
                if (ax * ax + ay * ay).sqrt() > ZOOM_DRAG_CLICK_SLOP {
                    self.zoom_dragged = true;
                }
            }
            // Vertical drag = scrubby zoom about the fixed press anchor (down zooms in, up out).
            let dy = pos.1 - self.zoom_last_y;
            self.zoom_last_y = pos.1;
            if dy != 0.0 {
                if let Some(dims) = self.view_dims() {
                    let factor = (dy * ZOOM_DRAG_SENSITIVITY).exp();
                    self.view
                        .zoom_to_cursor(factor, self.zoom_anchor, dims, &self.viewport);
                    self.refresh();
                }
            }
        }
    }

    pub fn begin_drag(&mut self) {
        self.dragging = true;
    }

    pub fn end_drag(&mut self) {
        self.dragging = false;
    }

    /// Begin an RMB zoom-drag, pivoting on the current cursor (the press point).
    pub fn begin_zoom_drag(&mut self) {
        self.zoom_dragging = true;
        self.zoom_anchor = self.cursor;
        self.zoom_last_y = self.cursor.1;
        self.zoom_dragged = false;
    }

    /// End an RMB gesture. Returns `true` if it was an actual zoom-drag (the cursor moved past
    /// [`ZOOM_DRAG_CLICK_SLOP`]); `false` if it was effectively a right-click, so the caller can
    /// open the context menu instead.
    pub fn end_zoom_drag(&mut self) -> bool {
        self.zoom_dragging = false;
        self.zoom_dragged
    }

    /// Whether an RMB zoom-drag is in progress (the shell repaints the zoom % while it is).
    pub fn is_zoom_dragging(&self) -> bool {
        self.zoom_dragging
    }

    pub fn zoom_at_cursor(&mut self, factor: f32) {
        if let Some(dims) = self.view_dims() {
            self.view
                .zoom_to_cursor(factor, self.cursor, dims, &self.viewport);
            self.refresh();
        }
    }

    pub fn zoom_centered(&mut self, factor: f32) {
        if let Some(dims) = self.view_dims() {
            self.view.zoom_centered(factor, dims, &self.viewport);
            self.refresh();
        }
    }

    pub fn fit(&mut self) {
        if let Some(dims) = self.view_dims() {
            self.view
                .fit_to_window(dims, &self.viewport, self.fit_upscale);
            self.refresh();
        }
    }

    pub fn one_to_one(&mut self) {
        self.view.one_to_one();
        if let Some(dims) = self.view_dims() {
            self.view.clamp_pan(dims, &self.viewport);
        }
        self.refresh();
    }

    pub fn toggle_channel(&mut self, ch: Channel) {
        self.display.channel = if self.display.channel == ch {
            Channel::Rgb
        } else {
            ch
        };
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

    pub fn reset_exposure(&mut self) {
        self.display.exposure = 0.0;
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

fn channel_code(ch: Channel) -> i32 {
    match ch {
        Channel::Rgb => 0,
        Channel::R => 1,
        Channel::G => 2,
        Channel::B => 3,
        Channel::A => 4,
    }
}

/// The backdrop an image gets when the user hasn't pinned one (no toolbar pick, `background =
/// "auto"`): a checkerboard only when there is real transparency to read *as* transparency, solid
/// black otherwise. An RGBA/gray+A source whose alpha is entirely opaque (e.g. a 32-bit screenshot)
/// carries no transparency, so it gets black like an opaque image — but it keeps its true format
/// and an inspectable alpha channel (`alpha_opaque`); the user can still isolate the all-white
/// alpha.
fn default_background(img: &DecodedImage) -> Background {
    let has_transparency = matches!(img.channels, 2 | 4) && !img.alpha_opaque;
    if has_transparency {
        Background::Checker
    } else {
        Background::Black
    }
}

/// Backdrop mode → shader code (must match the `background` branch in `shader.hlsl`).
fn background_code(bg: Background) -> i32 {
    match bg {
        Background::Black => 0,
        Background::White => 1,
        Background::Grey => 2,
        Background::Checker => 3,
    }
}

/// sRGB→linear for a single component (matches the former CPU shader), used for the clear color.
fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Create a hardware D3D11 device, falling back to the WARP software rasterizer (RDP/headless).
fn create_device() -> (ID3D11Device, ID3D11DeviceContext) {
    let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
    for (driver, is_warp) in [
        (D3D_DRIVER_TYPE_HARDWARE, false),
        (D3D_DRIVER_TYPE_WARP, true),
    ] {
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        let r = unsafe {
            D3D11CreateDevice(
                None,
                driver,
                Default::default(),
                D3D11_CREATE_DEVICE_FLAG(0),
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        };
        if r.is_ok() {
            if is_warp {
                eprintln!("fire: no hardware D3D11 device — using WARP software renderer");
            }
            return (device.unwrap(), context.unwrap());
        }
    }
    panic!("fire: D3D11CreateDevice failed for both hardware and WARP");
}

/// Create the DXGI flip-model swapchain on `hwnd`. Backbuffer is plain `UNORM` (flip model
/// disallows `*_SRGB` swapchain formats); the sRGB encode is done by the `*_SRGB` RTV.
fn create_swapchain(device: &ID3D11Device, hwnd: HWND, w: u32, h: u32) -> IDXGISwapChain1 {
    let desc = DXGI_SWAP_CHAIN_DESC1 {
        Width: w,
        Height: h,
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 2,
        Scaling: DXGI_SCALING_STRETCH,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
        AlphaMode: DXGI_ALPHA_MODE_IGNORE,
        ..Default::default()
    };
    unsafe {
        let dxgi_device: IDXGIDevice = device.cast().expect("IDXGIDevice");
        let adapter: IDXGIAdapter = dxgi_device.GetAdapter().expect("GetAdapter");
        let factory: IDXGIFactory2 = adapter.GetParent().expect("IDXGIFactory2");
        factory
            .CreateSwapChainForHwnd(device, hwnd, &desc, None, None)
            .expect("CreateSwapChainForHwnd")
    }
}

/// Create the vertex + pixel shaders from the DXBC that `fxc` precompiled at build time (see
/// `build.rs`); the bytecode is embedded in the exe, so there is no runtime HLSL compile.
fn create_shaders(device: &ID3D11Device) -> (ID3D11VertexShader, ID3D11PixelShader) {
    const VS_DXBC: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/vs_main.dxbc"));
    const PS_DXBC: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/ps_main.dxbc"));
    unsafe {
        let mut vs: Option<ID3D11VertexShader> = None;
        let mut ps: Option<ID3D11PixelShader> = None;
        device
            .CreateVertexShader(VS_DXBC, None, Some(&mut vs))
            .expect("CreateVertexShader");
        device
            .CreatePixelShader(PS_DXBC, None, Some(&mut ps))
            .expect("CreatePixelShader");
        (vs.unwrap(), ps.unwrap())
    }
}

/// Two samplers: anisotropic+mips for minify, point for crisp magnify/1:1. Both clamp at edges.
fn create_samplers(device: &ID3D11Device) -> (ID3D11SamplerState, ID3D11SamplerState) {
    let base = D3D11_SAMPLER_DESC {
        Filter: D3D11_FILTER_ANISOTROPIC,
        AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
        AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
        AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
        MipLODBias: 0.0,
        MaxAnisotropy: 8,
        ComparisonFunc: D3D11_COMPARISON_NEVER,
        BorderColor: [0.0; 4],
        MinLOD: 0.0,
        MaxLOD: f32::MAX,
    };
    let point = D3D11_SAMPLER_DESC {
        Filter: D3D11_FILTER_MIN_MAG_MIP_POINT,
        MaxAnisotropy: 1,
        ..base
    };
    unsafe {
        let mut aniso: Option<ID3D11SamplerState> = None;
        let mut pt: Option<ID3D11SamplerState> = None;
        device
            .CreateSamplerState(&base, Some(&mut aniso))
            .expect("CreateSamplerState aniso");
        device
            .CreateSamplerState(&point, Some(&mut pt))
            .expect("CreateSamplerState point");
        (aniso.unwrap(), pt.unwrap())
    }
}

/// Create the dynamic per-frame constant buffer ([`Params`], 80 bytes, 16-byte aligned).
fn create_const_buffer(device: &ID3D11Device) -> ID3D11Buffer {
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: std::mem::size_of::<Params>() as u32,
        Usage: D3D11_USAGE_DYNAMIC,
        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
        MiscFlags: 0,
        StructureByteStride: 0,
    };
    unsafe {
        let mut buf: Option<ID3D11Buffer> = None;
        device
            .CreateBuffer(&desc, None, Some(&mut buf))
            .expect("CreateBuffer (constants)");
        buf.unwrap()
    }
}

/// Build a `DEFAULT` texture (+ SRV, with a GPU-generated mip chain) from one RGBA frame, returning
/// the texture, its sampling view, and the `linear_sample` flag for `format` (1 if the sample is
/// already linear — 8-bit `*_SRGB` / float — 0 if the shader must sRGB-decode 16-bit unorm). A free
/// function (not a method) so the per-frame animation upload can borrow pixels straight out of
/// `anim_frames` without aliasing the `&mut self` receiver. Returns the GPU error instead of
/// panicking (this runs synchronously in the wndproc, where a panic would abort the process).
fn create_image_texture(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    pixels: &[u8],
    width: u32,
    height: u32,
    format: PixelFormat,
) -> windows::core::Result<(ID3D11Texture2D, ID3D11ShaderResourceView, i32)> {
    let (dxgi_format, bpp, linear_sample) = match format {
        // 8-bit sources are sRGB-encoded; the `*_SRGB` view decodes to linear on sample.
        PixelFormat::Rgba8Unorm => (DXGI_FORMAT_R8G8B8A8_UNORM_SRGB, 4u32, 1i32),
        // 16-bit unorm is treated as sRGB-encoded (matches the CPU path) → decode in shader.
        PixelFormat::Rgba16Unorm => (DXGI_FORMAT_R16G16B16A16_UNORM, 8, 0),
        // Float sources are already linear.
        PixelFormat::Rgba16Float => (DXGI_FORMAT_R16G16B16A16_FLOAT, 8, 1),
        PixelFormat::Rgba32Float => (DXGI_FORMAT_R32G32B32A32_FLOAT, 16, 1),
    };

    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 0, // 0 → full chain; populated by GenerateMips below
        ArraySize: 1,
        Format: dxgi_format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: D3D11_RESOURCE_MISC_GENERATE_MIPS.0 as u32,
    };

    unsafe {
        let mut tex: Option<ID3D11Texture2D> = None;
        device.CreateTexture2D(&desc, None, Some(&mut tex))?;
        let tex = tex.unwrap();

        // Level 0 only; the rest are generated.
        context.UpdateSubresource(
            &tex,
            0,
            None,
            pixels.as_ptr() as *const c_void,
            width * bpp,
            0,
        );

        let mut srv: Option<ID3D11ShaderResourceView> = None;
        device.CreateShaderResourceView(&tex, None, Some(&mut srv))?;
        let srv = srv.unwrap();
        context.GenerateMips(&srv);

        Ok((tex, srv, linear_sample))
    }
}
