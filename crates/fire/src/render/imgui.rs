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

pub struct Imgui {
    ctx: Context,
    /// The icon atlas. ImGui refers to it by raw SRV pointer, so both must outlive every frame that
    /// references them — hence they are owned here and only ever replaced wholesale.
    _icon_tex: Option<ID3D11Texture2D>,
    icon_srv: Option<ID3D11ShaderResourceView>,
    icon_id: TextureId,
    /// Physical edge the atlas was last rastered at, so [`Imgui::refresh_icons`] can tell whether a
    /// DPI or stylesheet change actually moved it.
    icon_built_px: f32,
    dpi: u32,
    /// ImGui's factory style, captured at creation *before* [`crate::ui::theme`] overwrites it.
    /// The settings window is drawn with this — see [`StockStyle`].
    stock: sys::ImGuiStyle,
}

/// A second, independent ImGui style — the settings window's.
///
/// It starts from ImGui's *factory* style (captured before `ui::theme` overwrites the live one, which
/// is the only moment it exists), because that is a **form** geometry: visible buttons, framed inputs,
/// sane padding. The chrome's style is a *toolbar* — transparent buttons, tight spacing, tuned to sit
/// over an image — and a dialog that inherits it has invisible buttons and no field frames.
///
/// The caller then paints it: [`Self::style_mut`] hands it to [`crate::ui::theme::form`], which
/// applies the stylesheet's palette on top. So the settings window shares the app's *colors* without
/// inheriting the toolbar's *shape*.
#[derive(Clone, Copy)]
pub struct FormStyle(sys::ImGuiStyle);

impl FormStyle {
    /// Mutable access, so the UI layer can theme it. `Style` is a `#[repr(transparent)]` wrapper over
    /// `ImGuiStyle` (the crate asserts the layout), which keeps every color decision in `ui::theme`
    /// and out of this FFI module.
    pub fn style_mut(&mut self) -> &mut dear_imgui_rs::Style {
        // SAFETY: layout-compatible by the wrapper's own const assertions.
        unsafe { &mut *(&mut self.0 as *mut sys::ImGuiStyle as *mut dear_imgui_rs::Style) }
    }

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

/// Seed a style's colors from ImGui's factory palette for the mode, before [`crate::ui::theme`]
/// paints ours over the top.
///
/// Both styles need this, for the same reason. The stylesheet names the colors fire actually uses;
/// ImGui has *dozens* more (plots, tables, drag-drop, nav, text selection). Without a seed, those
/// keep whatever was in the style already — for the chrome that is the factory **dark** palette, from
/// context creation, *whatever mode the user is in*. So a light-mode window would draw its text
/// selection, its nav cursor and its resize grips out of a dark palette, and any color we later stop
/// naming would silently freeze at an ImGui default. Seeding makes "unnamed" mean "ImGui's sensible
/// value for this mode" instead of "a stale value from startup".
///
/// (`FormStyle` seeds itself the same way in [`Imgui::form_style`] — this is that, for the live style.)
pub fn seed_colors(style: &mut dear_imgui_rs::Style, dark: bool) {
    // SAFETY: `Style` is `#[repr(transparent)]` over `ImGuiStyle` (the crate asserts the layout), and
    // `igStyleColors*` only writes the color array — the metrics and font fields are untouched.
    let raw = style as *mut dear_imgui_rs::Style as *mut sys::ImGuiStyle;
    unsafe {
        if dark {
            sys::igStyleColorsDark(raw);
        } else {
            sys::igStyleColorsLight(raw);
        }
    }
}

/// Center the next window on the client, the first time it appears. `Appearing`, not `Always`, so
/// the user can drag it somewhere else and it stays there.
pub fn center_next_window(client: (f32, f32)) {
    let center = sys::ImVec2_c {
        x: client.0 * 0.5,
        y: client.1 * 0.5,
    };
    place(
        center,
        sys::ImVec2_c { x: 0.5, y: 0.5 },
        sys::ImGuiCond_Appearing,
    );
}

/// Put the next window's top-left at `pos` (client coords) when it appears — how a popup menu is
/// anchored to the cursor, or dropped from under the button that opened it. Left to itself, ImGui
/// would place a popup at the mouse, which is *nearly* right for a toolbar button and visibly wrong
/// for anything else.
pub fn position_next_window(pos: (f32, f32)) {
    let p = sys::ImVec2_c { x: pos.0, y: pos.1 };
    place(p, sys::ImVec2_c { x: 0.0, y: 0.0 }, sys::ImGuiCond_Appearing);
}

/// Anchor the next window at `pos` with `pivot` (`0.0` = leading edge, `0.5` = centered, `1.0` =
/// trailing) on each axis, **every frame**.
///
/// This is what lets a window be auto-sized *and* centered: with `ALWAYS_AUTO_RESIZE`, ImGui measures
/// the content, and the pivot places that measured box — so nothing has to compute a width in order
/// to halve it. `Always`, not `Appearing`, because the anchor is derived from the layout (the image's
/// sub-rect) and has to follow a resize.
pub fn anchor_next_window(pos: (f32, f32), pivot: (f32, f32)) {
    let p = sys::ImVec2_c { x: pos.0, y: pos.1 };
    let v = sys::ImVec2_c {
        x: pivot.0,
        y: pivot.1,
    };
    place(p, v, sys::ImGuiCond_Always);
}

fn place(pos: sys::ImVec2_c, pivot: sys::ImVec2_c, cond: sys::ImGuiCond_) {
    unsafe { sys::igSetNextWindowPos(pos, cond as sys::ImGuiCond, pivot) };
}

/// Size the next window, **the first time it appears** — so it opens proportioned to the viewport it
/// is opening over, and stays wherever the user then drags or resizes it to.
pub fn size_next_window(size: (f32, f32)) {
    let s = sys::ImVec2_c {
        x: size.0,
        y: size.1,
    };
    unsafe { sys::igSetNextWindowSize(s, sys::ImGuiCond_Appearing as sys::ImGuiCond) };
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
            icon_built_px: 0.0,
            dpi: dpi.max(96),
            stock,
        };
        me.refresh_icons(device);
        me
    }

    /// The settings window's base style: ImGui's factory geometry, scaled for the monitor, carrying
    /// our font — for [`crate::ui::theme::form`] to paint and [`FormStyle::push`] to install.
    ///
    /// Composed per call rather than cached: it is a ~1 KB POD copy plus two library calls, against a
    /// frame that is only drawn when something happened. Caching it would mean invalidating it on DPI
    /// *and* theme changes, which is more state than it saves.
    pub fn form_style(&self, dark: bool) -> FormStyle {
        let mut s = self.stock;
        // Factory geometry, scaled to the monitor. `ui::theme::form` then overrides the metrics it
        // cares about; the rest (cell padding, separator-text padding, …) stay correctly scaled.
        unsafe { sys::ImGuiStyle_ScaleAllSizes(&mut s, self.dpi as f32 / 96.0) };
        // Seeds every color, including the dozens the theme doesn't name (plots, drag-drop, tables).
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
        FormStyle(s)
    }

    /// Physical icon edge for the current DPI. The *logical* edge is a stylesheet value
    /// (`[font] icon_size` in `ui/theme.toml`), so this moves on a hot reload as well as on a DPI
    /// change — which is what [`Imgui::refresh_icons`] is for.
    pub fn icon_px(&self) -> f32 {
        (crate::ui::theme::current().font.icon_size * self.dpi as f32 / 96.0).round()
    }

    pub fn style_mut(&mut self) -> &mut dear_imgui_rs::Style {
        self.ctx.style_mut()
    }

    /// Adopt a new DPI. ImGui 1.92 re-bakes *glyphs* lazily, so there is no font atlas to rebuild
    /// and nothing else to do here; the icon atlas is a real raster and is the caller's next call
    /// ([`Imgui::refresh_icons`]), and the style (including `font_scale_dpi`) goes through
    /// [`crate::ui::theme::apply`] — the one place metrics are decided.
    pub fn set_dpi(&mut self, dpi: u32) {
        self.dpi = dpi.max(96);
    }

    /// Re-raster the icon atlas if the physical icon size has moved — a DPI change, or a stylesheet
    /// edit. Cheap no-op when it hasn't, so the restyle path can call it unconditionally.
    pub fn refresh_icons(&mut self, device: &ID3D11Device) {
        if self.icon_px() != self.icon_built_px {
            self.rebuild_icons(device);
        }
    }

    fn rebuild_icons(&mut self, device: &ID3D11Device) {
        let px = self.icon_px();
        let n = px as usize;
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
        self.icon_built_px = px;
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
