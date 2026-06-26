//! GPU viewport: a Direct3D 11 renderer that presents the decoded image through a DXGI
//! flip-model swapchain on the child "view" window. This replaced the former pure-CPU
//! softbuffer renderer — the image lives as a GPU texture (with a hardware mip chain), and
//! pan/zoom/exposure/channel/tonemap are just constant-buffer values, so each frame is one
//! textured fullscreen triangle: the per-frame CPU cost is ~a 80-byte upload + a draw call,
//! and the GPU does the sampling/color pipeline the CPU shader used to do per pixel.
//!
//! Why this is smoother *and* lower-CPU than the CPU path: panning no longer re-runs the
//! per-pixel pipeline on the CPU — it changes a transform and the GPU re-samples the texture.
//! Presentation is vsync-paced through the flip-model swapchain (tear-free at high refresh)
//! instead of an unsynchronized GDI BitBlt.
//!
//! Color correctness matches the former CPU shader: 8-bit sources upload as `*_UNORM_SRGB` (hardware
//! sRGB→linear on sample), float sources are already linear, 16-bit unorm is sRGB-decoded in
//! the shader. The pixel shader outputs **linear** and the render-target view is `*_SRGB`, so
//! the hardware sRGB-encodes on write — matching the CPU encode. The whole pipeline is linear.

use std::ffi::c_void;

use fire_decode::{DecodedImage, PixelFormat};

use windows::core::{Interface, PCSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{
    ID3DBlob, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_11_0,
    D3D_FEATURE_LEVEL_11_1, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Buffer, ID3D11Device, ID3D11DeviceContext, ID3D11PixelShader,
    ID3D11RenderTargetView, ID3D11SamplerState, ID3D11ShaderResourceView, ID3D11Texture2D,
    ID3D11VertexShader, D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_BUFFER_DESC, D3D11_COMPARISON_NEVER, D3D11_CPU_ACCESS_WRITE,
    D3D11_CREATE_DEVICE_FLAG, D3D11_FILTER_ANISOTROPIC, D3D11_FILTER_MIN_MAG_MIP_POINT,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_WRITE_DISCARD, D3D11_RENDER_TARGET_VIEW_DESC,
    D3D11_RENDER_TARGET_VIEW_DESC_0, D3D11_RESOURCE_MISC_GENERATE_MIPS, D3D11_RTV_DIMENSION_TEXTURE2D,
    D3D11_SAMPLER_DESC, D3D11_SDK_VERSION, D3D11_TEX2D_RTV, D3D11_TEXTURE2D_DESC,
    D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_USAGE_DEFAULT, D3D11_USAGE_DYNAMIC, D3D11_VIEWPORT,
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

use crate::render::view::{Channel, DisplayState, Tonemap, ViewState, Viewport};

/// Scrubby-zoom sensitivity: an RMB vertical drag multiplies zoom by `exp(dy * this)` per pixel
/// (~2.7× per 100 px). Exponential-in-pixels so the gesture feels uniform across the zoom range;
/// drag down (dy > 0) zooms in, up zooms out.
const ZOOM_DRAG_SENSITIVITY: f32 = 0.01;

/// Per-frame shader constants. Layout matches the HLSL `cbuffer` (16-byte float4 registers);
/// keep the field order/padding in lockstep with [`SHADER_HLSL`].
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
    _pad0: i32,
    _pad1: i32,
    _pad2: i32,
    clear_r: f32,
    clear_g: f32,
    clear_b: f32,
    clear_a: f32,
}

/// Vertex (fullscreen triangle from `SV_VertexID`) + pixel shader. The pixel shader is a direct
/// port of the former CPU shader's per-pixel pipeline: inverse-map the surface pixel into image
/// space, sample (point when magnifying for crisp texels, anisotropic+mips when minifying),
/// then exposure → tonemap → channel isolation → checker composite, all in linear light. The
/// `*_SRGB` render target handles the final sRGB encode.
const SHADER_HLSL: &str = r#"
Texture2D tex : register(t0);
SamplerState samp_aniso : register(s0);
SamplerState samp_point : register(s1);

cbuffer Params : register(b0) {
    float2 img_size;
    float2 surf_size;
    float2 pan;
    float  inv_zoom;
    float  exposure;
    int    channel;        // 0=RGB 1=R 2=G 3=B 4=A
    int    tonemap;        // 0=Reinhard 1=ACES
    int    is_hdr;
    int    has_image;
    int    linear_sample;  // 1=sample already linear, 0=sRGB-decode rgb in shader
    int3   _pad;
    float4 clear_lin;
};

struct VSOut { float4 pos : SV_Position; };

VSOut vs_main(uint vid : SV_VertexID) {
    float2 uv = float2((vid << 1) & 2, vid & 2); // (0,0) (2,0) (0,2)
    VSOut o;
    o.pos = float4(uv * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
    return o;
}

float3 srgb_to_linear(float3 c) {
    float3 lo = c / 12.92;
    float3 hi = pow(max((c + 0.055) / 1.055, 0.0), 2.4);
    return lerp(hi, lo, step(c, 0.04045));
}
float3 reinhard(float3 c) { return c / (1.0 + c); }
float3 aces(float3 x) {
    const float a = 2.51, b = 0.03, c = 2.43, d = 0.59, e = 0.14;
    return saturate((x * (a * x + b)) / (x * (c * x + d) + e));
}

float4 ps_main(float4 pos : SV_Position) : SV_Target {
    if (has_image == 0) return clear_lin;
    float2 sp = pos.xy;                       // surface pixel center (origin top-left)
    float2 ctr = surf_size * 0.5 + pan;
    float2 f = img_size * 0.5 + (sp - ctr) * inv_zoom;   // image texel coords
    if (f.x < 0.0 || f.y < 0.0 || f.x >= img_size.x || f.y >= img_size.y)
        return clear_lin;
    float2 uv = f / img_size;
    float4 s = (inv_zoom <= 1.0) ? tex.Sample(samp_point, uv)   // magnify/1:1 → crisp texels
                                 : tex.Sample(samp_aniso, uv);  // minify → mips + anisotropic
    float3 rgb = s.rgb;
    float a = s.a;
    if (linear_sample == 0) rgb = srgb_to_linear(rgb);
    if (is_hdr != 0) {
        rgb *= exposure;
        rgb = (tonemap == 1) ? aces(rgb) : reinhard(rgb);
    }
    if (channel == 1) return float4(rgb.rrr, 1.0);
    if (channel == 2) return float4(rgb.ggg, 1.0);
    if (channel == 3) return float4(rgb.bbb, 1.0);
    if (channel == 4) { float v = srgb_to_linear(float3(a, a, a)).x; return float4(v, v, v, 1.0); }
    if (a < 0.999) {
        float2 cell = floor(sp / 12.0);
        float bg = (fmod(cell.x + cell.y, 2.0) < 0.5) ? 0.45 : 0.21;
        rgb = bg * (1.0 - a) + rgb * a;
    }
    return float4(rgb, 1.0);
}
"#;

/// GPU render state for the view window: the D3D11 device/swapchain plus the same pan/zoom/fit
/// + channel/exposure/tonemap state the CPU surface carried (so the window shell and chrome
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

    /// Letterbox / no-image backdrop, packed sRGB and its linear form (for the `*_SRGB` RTV).
    clear: u32,
    clear_lin: [f32; 4],

    /// Monotonic per-window decode generation; a `DecodeDone` older than this is stale.
    generation: u64,
    /// The displayed image — retained for the pixel inspector (#16) and for re-fit on resize.
    current_image: Option<DecodedImage>,

    viewport: Viewport,
    view: ViewState,
    display: DisplayState,
    cursor: (f32, f32),
    dragging: bool,
    /// RMB scrubby-zoom: whether a zoom-drag is active, the pivot (the press point, surface px),
    /// and the last cursor-y so each move applies an incremental zoom.
    zoom_dragging: bool,
    zoom_anchor: (f32, f32),
    zoom_last_y: f32,
}

impl GpuSurface {
    /// Build the D3D11 device + flip-model swapchain on the child view HWND. `_hinstance` is
    /// unused (D3D needs only the HWND); kept for signature parity with the CPU surface.
    pub fn new(hwnd: isize, _hinstance: isize, width: u32, height: u32) -> Self {
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
            generation: 0,
            current_image: None,
            viewport: Viewport::new(width, height),
            view: ViewState::default(),
            display: DisplayState::default(),
            cursor: (0.0, 0.0),
            dragging: false,
            zoom_dragging: false,
            zoom_anchor: (0.0, 0.0),
            zoom_last_y: 0.0,
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
        self.current_image.as_ref()
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
        self.current_image.as_ref().is_some_and(|i| i.format.is_hdr())
    }

    fn image_dims(&self) -> Option<(u32, u32)> {
        self.current_image.as_ref().map(|i| (i.width, i.height))
    }

    /// Drop the displayed image so the next paint shows the placeholder.
    pub fn clear_image(&mut self) {
        self.current_image = None;
        self._tex = None;
        self.srv = None;
    }

    /// Adopt a decoded image: upload it as a GPU texture (hardware mip chain) and reset to fit +
    /// neutral display state for the new file (#17).
    pub fn set_image(&mut self, img: DecodedImage) {
        let (w, h) = (img.width, img.height);
        self.upload_texture(&img);
        self.current_image = Some(img);
        self.display = DisplayState::default();
        self.view.fit_to_window((w, h), &self.viewport);
    }

    /// Upload `img` as a `DEFAULT` texture with a full mip chain generated on the GPU.
    fn upload_texture(&mut self, img: &DecodedImage) {
        let (format, bpp, linear_sample) = match img.format {
            // 8-bit sources are sRGB-encoded; the `*_SRGB` view decodes to linear on sample.
            PixelFormat::Rgba8Unorm => (DXGI_FORMAT_R8G8B8A8_UNORM_SRGB, 4u32, 1i32),
            // 16-bit unorm is treated as sRGB-encoded (matches the CPU path) → decode in shader.
            PixelFormat::Rgba16Unorm => (DXGI_FORMAT_R16G16B16A16_UNORM, 8, 0),
            // Float sources are already linear.
            PixelFormat::Rgba16Float => (DXGI_FORMAT_R16G16B16A16_FLOAT, 8, 1),
            PixelFormat::Rgba32Float => (DXGI_FORMAT_R32G32B32A32_FLOAT, 16, 1),
        };
        self.linear_sample = linear_sample;

        let desc = D3D11_TEXTURE2D_DESC {
            Width: img.width,
            Height: img.height,
            MipLevels: 0, // 0 → full chain; populated by GenerateMips below
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: D3D11_RESOURCE_MISC_GENERATE_MIPS.0 as u32,
        };

        unsafe {
            let mut tex: Option<ID3D11Texture2D> = None;
            self.device
                .CreateTexture2D(&desc, None, Some(&mut tex))
                .expect("CreateTexture2D");
            let tex = tex.unwrap();

            // Level 0 only; the rest are generated.
            self.context.UpdateSubresource(
                &tex,
                0,
                None,
                img.pixels.as_ptr() as *const c_void,
                img.width * bpp,
                0,
            );

            let mut srv: Option<ID3D11ShaderResourceView> = None;
            self.device
                .CreateShaderResourceView(&tex, None, Some(&mut srv))
                .expect("CreateShaderResourceView");
            let srv = srv.unwrap();
            self.context.GenerateMips(&srv);

            self._tex = Some(tex);
            self.srv = Some(srv);
        }
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
            let _ = self.swapchain.ResizeBuffers(
                0,
                width,
                height,
                DXGI_FORMAT_UNKNOWN,
                DXGI_SWAP_CHAIN_FLAG(0),
            );
        }
        if let Some(dims) = self.image_dims() {
            if self.view.fit {
                self.view.fit_to_window(dims, &self.viewport);
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
    /// shader's linear output is sRGB-encoded on write.
    fn ensure_rtv(&mut self) {
        if self.rtv.is_some() {
            return;
        }
        unsafe {
            let back: ID3D11Texture2D = self.swapchain.GetBuffer(0).expect("swapchain GetBuffer");
            let desc = D3D11_RENDER_TARGET_VIEW_DESC {
                Format: DXGI_FORMAT_R8G8B8A8_UNORM_SRGB,
                ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_RTV { MipSlice: 0 },
                },
            };
            let mut rtv: Option<ID3D11RenderTargetView> = None;
            self.device
                .CreateRenderTargetView(&back, Some(&desc), Some(&mut rtv))
                .expect("CreateRenderTargetView");
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
        let (img_w, img_h, has_image) = match &self.current_image {
            Some(img) => (img.width as f32, img.height as f32, 1),
            None => (1.0, 1.0, 0),
        };
        let params = Params {
            img_w,
            img_h,
            surf_w: self.viewport.width,
            surf_h: self.viewport.height,
            pan_x: self.view.pan.0,
            pan_y: self.view.pan.1,
            inv_zoom: 1.0 / self.view.zoom,
            exposure: if is_hdr { self.display.exposure.exp2() } else { 1.0 },
            channel: channel_code(self.display.channel),
            tonemap: match self.display.tonemap {
                Tonemap::Reinhard => 0,
                Tonemap::Aces => 1,
            },
            is_hdr: is_hdr as i32,
            has_image,
            linear_sample: self.linear_sample,
            _pad0: 0,
            _pad1: 0,
            _pad2: 0,
            clear_r: self.clear_lin[0],
            clear_g: self.clear_lin[1],
            clear_b: self.clear_lin[2],
            clear_a: 1.0,
        };

        unsafe {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            if self
                .context
                .Map(&self.cbuffer, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))
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
            self.context.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
            self.context.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            self.context.VSSetShader(&self.vs, None);
            self.context.PSSetShader(&self.ps, None);
            self.context.PSSetConstantBuffers(0, Some(&[Some(self.cbuffer.clone())]));
            self.context.PSSetShaderResources(0, Some(&[self.srv.clone()]));
            self.context
                .PSSetSamplers(0, Some(&[Some(self.samp_aniso.clone()), Some(self.samp_point.clone())]));
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
            if let Some(dims) = self.image_dims() {
                self.view.pan_by(delta, dims, &self.viewport);
                self.refresh();
            }
        } else if self.zoom_dragging {
            // Vertical drag = scrubby zoom about the fixed press anchor (down zooms in, up out).
            let dy = pos.1 - self.zoom_last_y;
            self.zoom_last_y = pos.1;
            if dy != 0.0 {
                if let Some(dims) = self.image_dims() {
                    let factor = (dy * ZOOM_DRAG_SENSITIVITY).exp();
                    self.view.zoom_to_cursor(factor, self.zoom_anchor, dims, &self.viewport);
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
    }

    pub fn end_zoom_drag(&mut self) {
        self.zoom_dragging = false;
    }

    /// Whether an RMB zoom-drag is in progress (the shell repaints the zoom % while it is).
    pub fn is_zoom_dragging(&self) -> bool {
        self.zoom_dragging
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

fn channel_code(ch: Channel) -> i32 {
    match ch {
        Channel::Rgb => 0,
        Channel::R => 1,
        Channel::G => 2,
        Channel::B => 3,
        Channel::A => 4,
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
    for (driver, is_warp) in [(D3D_DRIVER_TYPE_HARDWARE, false), (D3D_DRIVER_TYPE_WARP, true)] {
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
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
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

/// Compile the HLSL and create the vertex + pixel shaders.
fn create_shaders(device: &ID3D11Device) -> (ID3D11VertexShader, ID3D11PixelShader) {
    let vs_blob = compile(c"vs_main", c"vs_5_0");
    let ps_blob = compile(c"ps_main", c"ps_5_0");
    unsafe {
        let vs_bytes =
            std::slice::from_raw_parts(vs_blob.GetBufferPointer() as *const u8, vs_blob.GetBufferSize());
        let ps_bytes =
            std::slice::from_raw_parts(ps_blob.GetBufferPointer() as *const u8, ps_blob.GetBufferSize());
        let mut vs: Option<ID3D11VertexShader> = None;
        let mut ps: Option<ID3D11PixelShader> = None;
        device.CreateVertexShader(vs_bytes, None, Some(&mut vs)).expect("CreateVertexShader");
        device.CreatePixelShader(ps_bytes, None, Some(&mut ps)).expect("CreatePixelShader");
        (vs.unwrap(), ps.unwrap())
    }
}

/// Compile one entry point of [`SHADER_HLSL`] to DXBC via the runtime `D3DCompile`.
fn compile(entry: &std::ffi::CStr, target: &std::ffi::CStr) -> ID3DBlob {
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    let r = unsafe {
        D3DCompile(
            SHADER_HLSL.as_ptr() as *const c_void,
            SHADER_HLSL.len(),
            PCSTR::null(),
            None,
            None,
            PCSTR(entry.as_ptr() as *const u8),
            PCSTR(target.as_ptr() as *const u8),
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if r.is_err() {
        let msg = errors
            .map(|e| unsafe {
                let bytes = std::slice::from_raw_parts(e.GetBufferPointer() as *const u8, e.GetBufferSize());
                String::from_utf8_lossy(bytes).into_owned()
            })
            .unwrap_or_default();
        panic!("fire: HLSL compile failed ({}): {msg}", entry.to_string_lossy());
    }
    code.unwrap()
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
        device.CreateSamplerState(&base, Some(&mut aniso)).expect("CreateSamplerState aniso");
        device.CreateSamplerState(&point, Some(&mut pt)).expect("CreateSamplerState point");
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
        device.CreateBuffer(&desc, None, Some(&mut buf)).expect("CreateBuffer (constants)");
        buf.unwrap()
    }
}
