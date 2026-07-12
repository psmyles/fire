//! The Dear ImGui layer: context, the two upstream backends, and the icon atlas texture.
//!
//! Lives in `render/` because it is the *other* place that legitimately touches the typed `windows`
//! crate — it hands D3D11 device/context pointers to the DX11 backend and builds the icon texture.
//! Everything above it (`crate::ui`) is pure immediate-mode UI code with no Win32 or COM in sight.
//!
//! **We own no backend code.** `dear-imgui-sys`'s `backend-shim-win32` / `backend-shim-dx11`
//! features compile ocornut's own `imgui_impl_win32.cpp` / `imgui_impl_dx11.cpp` and expose them
//! over the C ABI declared below. That is the whole reason this dependency is acceptable: the
//! platform/renderer glue — historically the part that rots — is upstream's problem, not ours.
//!
//! Two things here are load-bearing and easy to get wrong:
//!
//! * **sRGB.** The image pass renders through an `*_SRGB` RTV (the shader emits linear light).
//!   ImGui's colors are *already* sRGB, so drawing it through that same view would double-encode and
//!   wash the entire UI out. The UI pass therefore binds a second, plain-`UNORM` view of the very
//!   same backbuffer — see [`crate::render::gpu::GpuSurface::begin_frame`].
//! * **DPI.** ImGui 1.92's dynamic font system rasterizes glyphs on first use, so a DPI change is
//!   just `set_font_scale_dpi` — there is no atlas to rebuild. Only the icon texture (a real
//!   raster) gets rebuilt, in [`Imgui::set_dpi`].

use std::ffi::c_void;

use dear_imgui_rs::{Context, FontSource, TextureId, Ui};
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11ShaderResourceView, ID3D11Texture2D,
    D3D11_BIND_SHADER_RESOURCE, D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC, D3D11_USAGE_IMMUTABLE,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC};

use crate::icons;

// ocornut's Win32 + DX11 backends, compiled by dear-imgui-sys. Declared, never implemented.
unsafe extern "C" {
    fn dear_imgui_backend_win32_init(hwnd: *mut c_void) -> bool;
    fn dear_imgui_backend_win32_new_frame();
    fn dear_imgui_backend_win32_wnd_proc_handler(
        hwnd: *mut c_void,
        msg: u32,
        wparam: usize,
        lparam: isize,
    ) -> isize;
    fn dear_imgui_backend_win32_shutdown();
    fn dear_imgui_backend_dx11_init(device: *mut c_void, device_context: *mut c_void) -> bool;
    fn dear_imgui_backend_dx11_new_frame();
    fn dear_imgui_backend_dx11_render_draw_data(draw_data: *mut c_void);
    fn dear_imgui_backend_dx11_shutdown();
}

/// Logical (96-dpi) icon edge. Scaled by DPI to get the physical raster size.
const ICON_LOGICAL_PX: f32 = 16.0;

pub struct Imgui {
    ctx: Context,
    /// The icon atlas. ImGui refers to it by raw SRV pointer, so both must outlive every frame that
    /// references them — hence they are owned here and only ever replaced wholesale.
    _icon_tex: Option<ID3D11Texture2D>,
    icon_srv: Option<ID3D11ShaderResourceView>,
    icon_id: TextureId,
    dpi: u32,
}

impl Imgui {
    pub fn new(hwnd: isize, device: &ID3D11Device, device_ctx: &ID3D11DeviceContext, dpi: u32) -> Self {
        let mut ctx = Context::create();
        // No imgui.ini: fire has no dockspaces or user-arranged windows to persist, and a settings
        // file that rewrites itself on a timer would break the "an idle window costs ~0" invariant.
        let _ = ctx.set_ini_filename(None::<std::path::PathBuf>);

        // Segoe UI, to match the rest of the desktop. Registering the font costs ~0.4 ms and bakes
        // no glyphs (1.92 rasterizes on first draw); if it is somehow missing, ImGui's built-in
        // font stands in rather than the app failing to start.
        if let Ok(ttf) = std::fs::read(r"C:\Windows\Fonts\segoeui.ttf") {
            ctx.fonts().add_font(&[FontSource::TtfData {
                data: &ttf,
                size_pixels: None, // dynamic: sized per-frame from the style below
                config: None,
            }]);
        }

        unsafe {
            dear_imgui_backend_win32_init(hwnd as *mut c_void);
            dear_imgui_backend_dx11_init(
                device.as_raw(),
                device_ctx.as_raw(),
            );
        }

        let mut me = Imgui {
            ctx,
            _icon_tex: None,
            icon_srv: None,
            icon_id: TextureId::new(0),
            dpi: 0,
        };
        me.set_dpi(device, dpi);
        me
    }

    /// Physical icon edge for the current DPI.
    pub fn icon_px(&self) -> f32 {
        (ICON_LOGICAL_PX * self.dpi as f32 / 96.0).round()
    }


    pub fn style_mut(&mut self) -> &mut dear_imgui_rs::Style {
        self.ctx.style_mut()
    }

    /// Adopt a new DPI by re-rastering the icon atlas at the new physical size — the only real work,
    /// since ImGui 1.92 re-bakes glyphs lazily. The *style* (including `font_scale_dpi`) is the
    /// caller's job: it goes through [`crate::ui::theme::apply`], the one place metrics are decided.
    /// No-op if the DPI hasn't moved.
    pub fn set_dpi(&mut self, device: &ID3D11Device, dpi: u32) {
        let dpi = dpi.max(96);
        if dpi == self.dpi {
            return;
        }
        self.dpi = dpi;
        self.rebuild_icons(device);
    }

    fn rebuild_icons(&mut self, device: &ID3D11Device) {
        let n = self.icon_px() as usize;
        let (pixels, w) = icons::atlas(n);

        let desc = D3D11_TEXTURE2D_DESC {
            Width: w as u32,
            Height: n as u32,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_IMMUTABLE,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let init = D3D11_SUBRESOURCE_DATA {
            pSysMem: pixels.as_ptr() as *const c_void,
            SysMemPitch: (w * 4) as u32,
            SysMemSlicePitch: 0,
        };

        let mut tex: Option<ID3D11Texture2D> = None;
        let mut srv: Option<ID3D11ShaderResourceView> = None;
        unsafe {
            if device
                .CreateTexture2D(&desc, Some(&init), Some(&mut tex))
                .is_err()
            {
                eprintln!("fire: icon atlas CreateTexture2D failed");
                return;
            }
            let Some(t) = tex.as_ref() else { return };
            if device
                .CreateShaderResourceView(t, None, Some(&mut srv))
                .is_err()
            {
                eprintln!("fire: icon atlas CreateShaderResourceView failed");
                return;
            }
        }

        // The DX11 backend takes ImTextureID to *be* the SRV pointer.
        self.icon_id = match srv.as_ref() {
            Some(s) => TextureId::new(s.as_raw() as u64),
            None => TextureId::new(0),
        };
        self._icon_tex = tex;
        self.icon_srv = srv;
    }

    /// Feed a Win32 message to ImGui. `true` means ImGui consumed it and the shell should not.
    pub fn wnd_proc(&mut self, hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> bool {
        unsafe {
            dear_imgui_backend_win32_wnd_proc_handler(hwnd as *mut c_void, msg, wparam, lparam) != 0
        }
    }

    /// True when a widget (not the image) owns the pointer — the toolbar, status bar, a popup.
    pub fn wants_mouse(&mut self) -> bool {
        self.ctx.io_mut().want_capture_mouse()
    }

    /// True when a text field has focus, so keys are typing rather than driving the viewer.
    pub fn wants_keyboard(&mut self) -> bool {
        self.ctx.io_mut().want_capture_keyboard()
    }

    /// Build and render one UI frame into whatever RTV is currently bound. The caller binds the
    /// **UNORM** view first (see the sRGB note above) and presents afterwards.
    pub fn frame<R>(&mut self, build: impl FnOnce(&Ui, TextureId) -> R) -> R {
        let icon_id = self.icon_id;
        unsafe {
            dear_imgui_backend_dx11_new_frame();
            dear_imgui_backend_win32_new_frame();
        }
        let ui = self.ctx.frame();
        let out = build(ui, icon_id);
        let draw_data = self.ctx.render();
        unsafe {
            dear_imgui_backend_dx11_render_draw_data(draw_data as *mut _ as *mut c_void);
        }
        out
    }
}

impl Drop for Imgui {
    fn drop(&mut self) {
        // Shut the backends down before the Context (they hold pointers into it). The D3D
        // device/SRV outlive this via their own COM refcounts.
        unsafe {
            dear_imgui_backend_dx11_shutdown();
            dear_imgui_backend_win32_shutdown();
        }
    }
}
