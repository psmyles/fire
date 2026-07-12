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
use dear_imgui_sys as sys;
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
    /// ImGui's factory style, captured at creation *before* [`crate::ui::theme`] overwrites it.
    /// The settings window is drawn with this — see [`StockStyle`].
    stock: sys::ImGuiStyle,
}

/// ImGui's stock style — what the library ships with, before fire's chrome theme.
///
/// The settings window wears this deliberately. The chrome is a *toolbar*: flat, transparent, tuned
/// to sit over an image. A settings window is a *form*, and stock ImGui already knows what a form
/// looks like. Two things are carried over from the live style, because "stock" must not mean
/// "wrong": the **font** (Segoe UI, registered once globally) and the **DPI scale** — an unscaled
/// dialog on a 200% monitor is a bug, not a default.
#[derive(Clone, Copy)]
pub struct StockStyle(sys::ImGuiStyle);

impl StockStyle {
    /// Install it until the guard drops.
    ///
    /// Assigning `ImGuiStyle` mid-frame is exactly how ImGui implements `PushStyleVar` itself: the
    /// struct is a POD, read at widget-submission time, so windows built before and after the guard
    /// are untouched.
    #[must_use]
    pub fn push(self) -> StyleGuard {
        // SAFETY: one context, made current at creation; we are inside a frame.
        unsafe {
            let live = sys::igGetStyle();
            let saved = *live;
            *live = self.0;
            StyleGuard(saved)
        }
    }
}

/// Restores the style [`StockStyle::push`] replaced.
pub struct StyleGuard(sys::ImGuiStyle);

impl Drop for StyleGuard {
    fn drop(&mut self) {
        unsafe { *sys::igGetStyle() = self.0 };
    }
}

/// Center the next window on the client, the first time it appears. `Appearing`, not `Always`, so
/// the user can drag it somewhere else and it stays there.
pub fn center_next_window(client: (f32, f32)) {
    let center = sys::ImVec2_c {
        x: client.0 * 0.5,
        y: client.1 * 0.5,
    };
    place(center, sys::ImVec2_c { x: 0.5, y: 0.5 });
}

/// Put the next window's top-left at `pos` (client coords) when it appears — how a popup menu is
/// anchored to the cursor, or dropped from under the button that opened it. Left to itself, ImGui
/// would place a popup at the mouse, which is *nearly* right for a toolbar button and visibly wrong
/// for anything else.
pub fn position_next_window(pos: (f32, f32)) {
    let p = sys::ImVec2_c { x: pos.0, y: pos.1 };
    place(p, sys::ImVec2_c { x: 0.0, y: 0.0 });
}

fn place(pos: sys::ImVec2_c, pivot: sys::ImVec2_c) {
    unsafe { sys::igSetNextWindowPos(pos, sys::ImGuiCond_Appearing as sys::ImGuiCond, pivot) };
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

        // Snapshot the factory style *now*, before `ui::theme::apply` runs over it — this is the
        // only moment it exists. SAFETY: `Context::create` made this context current.
        let stock = unsafe { *sys::igGetStyle() };

        let mut me = Imgui {
            ctx,
            _icon_tex: None,
            icon_srv: None,
            icon_id: TextureId::new(0),
            dpi: 0,
            stock,
        };
        me.set_dpi(device, dpi);
        me
    }

    /// ImGui's stock style, scaled for `dpi` and themed light/dark, ready to [`StockStyle::push`].
    ///
    /// Composed per call rather than cached: it is a ~1 KB POD copy plus two library calls, against
    /// a frame that is only drawn when something happened. Caching it would mean invalidating it on
    /// DPI *and* theme changes, which is more state than it saves.
    pub fn stock_style(&self, dark: bool) -> StockStyle {
        let mut s = self.stock;
        // Stock geometry, scaled to the monitor.
        unsafe { sys::ImGuiStyle_ScaleAllSizes(&mut s, self.dpi as f32 / 96.0) };
        unsafe {
            if dark {
                sys::igStyleColorsDark(&mut s);
            } else {
                sys::igStyleColorsLight(&mut s);
            }
        }
        // The font is ours (Segoe UI); its size and DPI scale come from the live style so the
        // settings window renders text exactly like the rest of the app.
        let live = self.ctx.style();
        s.FontSizeBase = live.font_size_base();
        s.FontScaleMain = live.font_scale_main();
        s.FontScaleDpi = live.font_scale_dpi();
        StockStyle(s)
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

    /// True when ImGui wants the keys — a focused text field, or an open popup.
    pub fn wants_keyboard(&mut self) -> bool {
        self.ctx.io_mut().want_capture_keyboard()
    }

    /// True while a text field is being edited. The *only* thing in fire that needs a repaint with
    /// no input behind it (the caret blink), so the shell arms a timer on it — and kills it the
    /// moment this goes false, or an idle window would stop being free.
    pub fn wants_text_input(&mut self) -> bool {
        self.ctx.io_mut().want_text_input()
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
