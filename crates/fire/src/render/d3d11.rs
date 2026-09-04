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

use windows::core::Interface;
use windows::Win32::Foundation::{DXGI_STATUS_OCCLUDED, HWND};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11RenderTargetView, ID3D11Texture2D,
    D3D11_CREATE_DEVICE_FLAG, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIAdapter, IDXGIDevice, IDXGIFactory2, IDXGISwapChain1, DXGI_PRESENT, DXGI_SCALING_STRETCH,
    DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_EFFECT_FLIP_DISCARD,
    DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window;

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
    pub fn raw_device(&self) -> *const c_void {
        self.device.as_raw()
    }

    /// The raw immediate-context pointer, for `sg_environment`.
    pub fn raw_context(&self) -> *const c_void {
        self.context.as_raw()
    }
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

    /// The render-target view of the current backbuffer, as the raw pointer `sg_swapchain`
    /// takes, creating it if a resize dropped it. `None` (logged) if the device refuses — a
    /// device-removed reset — in which case the frame is skipped rather than drawn into nothing.
    pub fn render_view(&mut self) -> Option<*const c_void> {
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

    /// Present the completed frame, vsync-paced (sync interval 1). Returns whether anyone is
    /// looking: DXGI answers `DXGI_STATUS_OCCLUDED` *immediately* when the window is hidden or
    /// fully covered, and a caller pacing playback on a present that no longer blocks would spin.
    pub fn present(&self) -> bool {
        // SAFETY: a live swapchain; sync interval 1 with no flags is always valid.
        unsafe { self.swapchain.Present(1, DXGI_PRESENT(0)) != DXGI_STATUS_OCCLUDED }
    }
}
