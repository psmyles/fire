//! The D3D11 device and the DXGI flip-model swapchain sokol_gfx draws through on Windows.
//!
//! sokol_gfx does not own a window: it is handed a device at `sg_setup` (through
//! `sg_environment`) and a render-target view per frame (through `sg_swapchain`), and the shell
//! owns everything around them. This module is that glue — the same device creation and the same
//! flip-model swapchain the Win32+D3D11 shell used, minus every drawing call, which is now
//! sokol_gfx's. It is the one place on Windows that names D3D11 or DXGI. The macOS twin will be a
//! `CAMetalLayer` on the winit window, handing sokol_gfx its `MTLDevice` and per-frame drawable.
//!
//! The backbuffer is plain `R8G8B8A8_UNORM` (flip model disallows `*_SRGB` swapchain formats);
//! the image shader encodes sRGB itself and Dear ImGui's colors are already sRGB, so nothing here
//! needs a second view. The device is created on the bring-up thread (see
//! [`crate::render::gpu::Gpu::start`]) and used from the main thread after the join: D3D11
//! devices are free-threaded, and only the main thread ever touches the immediate context.

use std::ffi::c_void;
use std::time::{Duration, Instant};

use sokol::gfx as sg;
use windows::core::Interface;
use windows::Win32::Foundation::{DXGI_STATUS_OCCLUDED, HWND};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11RenderTargetView, ID3D11Texture2D,
    D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_FLAG, D3D11_SDK_VERSION,
    D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC, D3D11_USAGE_IMMUTABLE,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT, DXGI_FORMAT_R16G16B16A16_FLOAT,
    DXGI_FORMAT_R16G16B16A16_UNORM, DXGI_FORMAT_R32G32B32A32_FLOAT, DXGI_FORMAT_R8G8B8A8_TYPELESS,
    DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIAdapter, IDXGIDevice, IDXGIFactory2, IDXGISwapChain1, DXGI_PRESENT, DXGI_SCALING_STRETCH,
    DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_EFFECT_FLIP_DISCARD,
    DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window;

use crate::render::mips::Convert;

/// What every window's backbuffer is created as, and what sokol_gfx is told to expect of a
/// swapchain pass, so the viewport and ImGui pipelines match it. Plain `RGBA8`: flip-model chains
/// disallow `*_SRGB` swapchain formats, and the image shader encodes sRGB itself (D20).
pub const SWAPCHAIN_FORMAT: sg::PixelFormat = sg::PixelFormat::Rgba8;

/// The process's D3D11 device and its immediate context — what sokol_gfx runs on.
pub struct Device {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
}

// SAFETY: a D3D11 device is free-threaded by specification. The immediate context is not, and it
// is only ever used by the main thread — the bring-up thread creates both and hands them over
// through a `JoinHandle`, whose join is the synchronization point.
unsafe impl Send for Device {}

impl Device {
    /// Create a hardware device, falling back to the WARP software rasterizer (RDP / headless).
    /// Errors (both drivers refused) come back as strings for the caller to show: this runs at
    /// startup in a process that may have no console, where a panic is an invisible abort.
    pub fn create() -> Result<Device, String> {
        let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
        let mut last_err = String::from("no D3D11 driver");
        for (driver, is_warp) in [
            (D3D_DRIVER_TYPE_HARDWARE, false),
            (D3D_DRIVER_TYPE_WARP, true),
        ] {
            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            // SAFETY: every out-pointer is a live local; the feature-level slice outlives the call.
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
            match r {
                Ok(()) => {
                    if is_warp {
                        eprintln!("fire: no hardware D3D11 device — using WARP software renderer");
                    }
                    // Success filled both out-params; the unwraps are unreachable by contract.
                    return Ok(Device {
                        device: device.unwrap(),
                        context: context.unwrap(),
                    });
                }
                Err(e) => last_err = format!("D3D11CreateDevice failed: {e}"),
            }
        }
        Err(last_err)
    }

    /// The raw `ID3D11Device` pointer, for `sg_environment`. Not retained by the caller beyond the
    /// device's own lifetime: sokol_gfx AddRefs what it keeps.
    fn raw_device(&self) -> *const c_void {
        self.device.as_raw()
    }

    /// The raw immediate-context pointer, for `sg_environment`.
    fn raw_context(&self) -> *const c_void {
        self.context.as_raw()
    }

    /// Point `env` at this device and its immediate context, so `sg_setup` runs on them.
    pub fn fill_environment(&self, env: &mut sg::Environment) {
        env.d3d11 = sg::D3d11Environment {
            device: self.raw_device(),
            device_context: self.raw_context(),
        };
    }

    /// A handle for building image textures on other threads.
    pub fn uploader(&self) -> Uploader {
        Uploader {
            device: self.device.clone(),
        }
    }
}

/// Builds image textures off the main thread — on the decode worker that decoded the image, so a
/// large upload never stalls the window.
///
/// Only the device is touched, which is free-threaded (it is created without
/// `D3D11_CREATE_DEVICE_SINGLETHREADED`); never the immediate context, and never sokol_gfx. The
/// finished texture is handed to sokol_gfx on the main thread ([`make_image`]).
#[derive(Clone)]
pub struct Uploader {
    device: ID3D11Device,
}

// SAFETY: a D3D11 device is free-threaded by specification; see `Device`.
unsafe impl Send for Uploader {}
unsafe impl Sync for Uploader {}

/// An image texture built by an [`Uploader`], owned until [`make_image`] hands it to sokol_gfx —
/// or until it is dropped, which frees it (a decode that was superseded on its way to the window).
pub struct NativeTexture(ID3D11Texture2D);

// SAFETY: D3D11 resources are free-threaded objects; nothing uses this one until it reaches the
// main thread.
unsafe impl Send for NativeTexture {}

/// sokol_gfx's pixel formats that image textures use, as the texture formats sokol_gfx itself
/// creates them with (`_sg_d3d11_texture_pixel_format`). sRGB8 is created *typeless*: sokol's
/// shader view names the sRGB format, and a view may only rename a typeless texture's format.
fn texture_format(format: sg::PixelFormat) -> Option<DXGI_FORMAT> {
    Some(match format {
        sg::PixelFormat::Srgb8a8 => DXGI_FORMAT_R8G8B8A8_TYPELESS,
        sg::PixelFormat::Rgba16 => DXGI_FORMAT_R16G16B16A16_UNORM,
        sg::PixelFormat::Rgba16f => DXGI_FORMAT_R16G16B16A16_FLOAT,
        sg::PixelFormat::Rgba32f => DXGI_FORMAT_R32G32B32A32_FLOAT,
        _ => return None,
    })
}

impl Uploader {
    /// Build a `width`×`height` texture of `format` with a full mip chain: level 0 from `level0`
    /// and the rest from `chain`, run here. `bpp` is bytes per texel *as uploaded*; `convert`, if
    /// any, turns source bytes into that format.
    ///
    /// The Metal twin overlaps level 0's copy with the chain's build. D3D11 cannot: the texture is
    /// immutable, as sokol_gfx's own are, so every level goes in at creation and the chain is
    /// built first. Whatever `chain` returned comes back alongside, so a caller whose texture
    /// failed can still upload the old way.
    #[allow(clippy::too_many_arguments)]
    pub fn upload<R: Send>(
        &self,
        width: u32,
        height: u32,
        format: sg::PixelFormat,
        bpp: usize,
        level0: &[u8],
        convert: Option<Convert>,
        chain: impl FnOnce() -> (Vec<Vec<u8>>, R) + Send,
    ) -> (Result<NativeTexture, String>, Vec<Vec<u8>>, R) {
        let (built, r) = chain();
        let tex = self.create(width, height, format, bpp, level0, &built, convert);
        (tex, built, r)
    }

    #[allow(clippy::too_many_arguments)]
    fn create(
        &self,
        width: u32,
        height: u32,
        format: sg::PixelFormat,
        bpp: usize,
        level0: &[u8],
        chain: &[Vec<u8>],
        convert: Option<Convert>,
    ) -> Result<NativeTexture, String> {
        let fmt = texture_format(format).ok_or_else(|| format!("no DXGI format for {format:?}"))?;
        let levels = crate::render::mips::level_count(width, height);
        if chain.len() + 1 != levels as usize {
            return Err("the mip chain is incomplete".into());
        }
        let converted: Vec<Vec<u8>> = match convert {
            Some(f) => std::iter::once(level0)
                .chain(chain.iter().map(Vec::as_slice))
                .map(f)
                .collect(),
            None => Vec::new(),
        };
        let data: Vec<&[u8]> = if convert.is_some() {
            converted.iter().map(Vec::as_slice).collect()
        } else {
            std::iter::once(level0)
                .chain(chain.iter().map(Vec::as_slice))
                .collect()
        };
        let init: Vec<D3D11_SUBRESOURCE_DATA> = data
            .iter()
            .enumerate()
            .map(|(level, bytes)| D3D11_SUBRESOURCE_DATA {
                pSysMem: bytes.as_ptr().cast(),
                SysMemPitch: ((width >> level).max(1) as usize * bpp) as u32,
                SysMemSlicePitch: 0,
            })
            .collect();
        // The description sokol_gfx gives an immutable image of its own (`_sg_d3d11_create_image`).
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: levels,
            ArraySize: 1,
            Format: fmt,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_IMMUTABLE,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut tex: Option<ID3D11Texture2D> = None;
        // SAFETY: `desc` and `init` (one entry per level, each pointing into a level that outlives
        // the call) are live locals; D3D11 copies the initial data before returning.
        let r = unsafe {
            self.device
                .CreateTexture2D(&desc, Some(init.as_ptr()), Some(&mut tex))
        };
        match (r, tex) {
            (Ok(()), Some(t)) => Ok(NativeTexture(t)),
            (Err(e), _) => Err(format!(
                "the GPU refused a {width}×{height} {format:?} texture with {levels} mip levels: {e}"
            )),
            (Ok(()), None) => Err("CreateTexture2D returned no texture".into()),
        }
    }
}

/// Hand `tex` to sokol_gfx as the image `desc` describes (its size, format and mip count must be
/// the texture's). sokol_gfx AddRefs an injected texture (`_sg_d3d11_create_image`), so ours is
/// released here and the image owns the texture from now on.
pub fn make_image(tex: NativeTexture, desc: &mut sg::ImageDesc) -> sg::Image {
    desc.d3d11_texture = tex.0.as_raw().cast_const();
    sg::make_image(desc)
}

/// One window's flip-model swapchain and the render-target view of its current backbuffer.
pub struct Swapchain {
    device: ID3D11Device,
    swapchain: IDXGISwapChain1,
    /// The backbuffer's view, created on demand ([`Self::render_view`]) and dropped on resize.
    rtv: Option<ID3D11RenderTargetView>,
    width: u32,
    height: u32,
}

impl Swapchain {
    /// Create a `width`×`height` swapchain on `window`'s client. Vsync-paced, two buffers,
    /// `FLIP_DISCARD`: the same chain the D3D11 shell presented through.
    pub fn new(device: &Device, window: &Window, width: u32, height: u32) -> Result<Self, String> {
        let hwnd = match window.window_handle().map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Win32(h)) => HWND(h.hwnd.get() as *mut c_void),
            _ => return Err("the window has no Win32 handle".into()),
        };
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: width.max(1),
            Height: height.max(1),
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
        // SAFETY: plain COM traversal device → adapter → factory on a live device; `desc` is
        // fully initialized and `hwnd` is the caller's live window.
        let swapchain = unsafe {
            let dxgi_device: IDXGIDevice = device.device.cast().map_err(|e| e.to_string())?;
            let adapter: IDXGIAdapter = dxgi_device.GetAdapter().map_err(|e| e.to_string())?;
            let factory: IDXGIFactory2 = adapter.GetParent().map_err(|e| e.to_string())?;
            factory
                .CreateSwapChainForHwnd(&device.device, hwnd, &desc, None, None)
                .map_err(|e| format!("CreateSwapChainForHwnd failed: {e}"))?
        };
        Ok(Swapchain {
            device: device.device.clone(),
            swapchain,
            rtv: None,
            width: width.max(1),
            height: height.max(1),
        })
    }

    /// The backbuffer size (physical px).
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Drop the view and resize the backbuffers. A zero dimension (a minimized window) is
    /// remembered but not applied — DXGI refuses it — and the frame is skipped instead.
    pub fn resize(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
        self.rtv = None;
        if width == 0 || height == 0 {
            return;
        }
        // SAFETY: the view — the only outstanding reference to a backbuffer — was released above.
        if let Err(e) = unsafe {
            self.swapchain.ResizeBuffers(
                0,
                width,
                height,
                DXGI_FORMAT_UNKNOWN,
                DXGI_SWAP_CHAIN_FLAG(0),
            )
        } {
            // The backbuffers stay at their old size; the next frame draws into them anyway.
            eprintln!("fire: swapchain ResizeBuffers failed: {e}");
        }
    }

    /// The twin of the Metal swapchain's `set_scale_factor`, and deliberately empty: a DXGI
    /// swapchain's buffers *are* the window's pixels, with no scale between the two for a DPI
    /// change to invalidate. The call still exists on both so the caller stays free of `cfg`.
    pub fn set_scale_factor(&mut self, scale: f64) {
        let _ = scale;
    }

    /// Acquire this frame's render target and point `sc` at it, returning whether there is a
    /// frame to draw. `false` (logged) means the device refused — a device-removed reset — and
    /// the frame is skipped rather than drawn into nothing.
    ///
    /// Nothing here waits on the display; a D3D11 frame blocks in [`Self::present`] instead.
    pub fn acquire(&mut self, sc: &mut sg::Swapchain) -> bool {
        match self.render_view() {
            Some(view) => {
                sc.d3d11.render_view = view;
                true
            }
            None => false,
        }
    }

    /// The render-target view of the current backbuffer, as the raw pointer `sg_swapchain`
    /// takes, creating it if a resize dropped it. `None` (logged) if the device refuses — a
    /// device-removed reset.
    fn render_view(&mut self) -> Option<*const c_void> {
        if self.rtv.is_none() {
            // SAFETY: buffer 0 of a live flip-model swapchain; the out-pointer is a live local.
            let made = unsafe {
                let back: ID3D11Texture2D = match self.swapchain.GetBuffer(0) {
                    Ok(b) => b,
                    Err(e) => {
                        eprintln!("fire: swapchain GetBuffer failed: {e}");
                        return None;
                    }
                };
                let mut rtv: Option<ID3D11RenderTargetView> = None;
                match self
                    .device
                    .CreateRenderTargetView(&back, None, Some(&mut rtv))
                {
                    Ok(()) => rtv,
                    Err(e) => {
                        eprintln!("fire: CreateRenderTargetView failed: {e}");
                        None
                    }
                }
            };
            self.rtv = made;
        }
        self.rtv
            .as_ref()
            .map(Interface::as_raw)
            .map(|p| p as *const c_void)
    }

    /// Present the completed frame, vsync-paced (sync interval 1), and report whether its handoff
    /// blocked on the display — the signal playback is paced on.
    ///
    /// Two ways it may not have blocked, and both must read as "did not wait" or a caller pacing
    /// on it would spin: DXGI answers `DXGI_STATUS_OCCLUDED` *immediately* when the window is
    /// hidden or fully covered, and a not-yet-full swapchain returns without waiting.
    pub fn present(&mut self) -> bool {
        let t0 = Instant::now();
        // SAFETY: a live swapchain; sync interval 1 with no flags is always valid.
        let looking = unsafe { self.swapchain.Present(1, DXGI_PRESENT(0)) != DXGI_STATUS_OCCLUDED };
        // A vblank is >= 4 ms even at 240 Hz; a present that did not wait returns in microseconds.
        looking && t0.elapsed() >= Duration::from_micros(500)
    }
}
