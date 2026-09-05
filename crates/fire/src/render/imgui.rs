//! The Dear ImGui layer: one context per window, its winit platform backend for input, sokol_imgui
//! as its renderer, and the two textures the UI draws from (the icon atlas, the empty-window logo).
//!
//! Lives in `render/` because it is the *other* place that legitimately names `sokol::gfx` — it
//! builds the textures and draws the UI into the frame's pass. Everything above it (`crate::ui`)
//! is pure immediate-mode UI code with no GPU API in sight.
//!
//! **We own no backend code.** Input comes through `dear-imgui-winit`, the maintained winit
//! backend of the same crate family as `dear-imgui-rs`, released in step with it. Drawing is
//! `sokol_imgui.h` (compiled by build.rs, see `simgui/`) in its `SOKOL_IMGUI_NO_SOKOL_APP` mode —
//! a renderer only: it uploads ImGui's textures (fonts included, through the 1.92 texture
//! protocol) as sokol_gfx images and draws the draw lists into the current pass. It is the same
//! header on every OS, maintained next to sokol_gfx by its author.
//!
//! **One context per window, and only one is ever current.** Dear ImGui has a single current
//! context; `dear-imgui-rs` models that as an active [`Context`] or a [`SuspendedContext`]. Every
//! window's context lives here *suspended*, and each operation activates it for exactly the
//! duration of a closure ([`Imgui::with`]) — so N windows in one process never race over the
//! global, and there is no ordering rule for the caller to remember. sokol_imgui keeps no context
//! of its own: `simgui_setup` runs once per process (it creates a throwaway context, destroyed on
//! the spot) and every call of its goes through `igGetIO()`, i.e. whatever is current — so it
//! draws whichever window's context is active, and tracks each context's textures through the
//! `ImTextureData` objects in that context's draw data. The one thing `dear-imgui-rs` does *not*
//! do here is render: its `render()` insists on one of its own renderer backends, so the frame is
//! opened through it (`Context::frame`, which is `igNewFrame`) and closed by `simgui_render`
//! (`igRender` + upload + draw). Its lifecycle bookkeeping reads ImGui's own frame state, so it
//! stays consistent.
//!
//! Three things here are load-bearing and easy to get wrong:
//!
//! * **sRGB.** The swapchain is written through a UNORM view, and ImGui's colors are already
//!   sRGB, so nothing here gamma-corrects; the image pass encodes its own output (see
//!   `shader.hlsl`). sokol_imgui picks its gamma from the swapchain format, which says UNORM.
//! * **DPI.** ImGui 1.92's dynamic font system rasterizes glyphs on first use, so a DPI change is
//!   just `set_font_scale_dpi` — there is no atlas to rebuild. Only the icon texture (a real
//!   raster) gets rebuilt, in [`Imgui::refresh_icons`]. The platform backend runs with its DPI
//!   handling *locked to 1.0*: the whole UI lays out in physical pixels (`ui::theme::Metrics`
//!   scales from the DPI itself), so ImGui's coordinate space must be the framebuffer's, not
//!   winit's logical one.
//! * **The frame closes even if the UI panics.** A panic mid-frame would leave the context between
//!   `NewFrame` and `Render`, and the next frame would assert. The build closure runs under
//!   `catch_unwind`; whatever it managed to build is rendered, the panic is logged, and the app
//!   carries on.

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use dear_imgui_rs::{
    render::SynchronousRendererConsumer, sys, BackendFlags, Context, FontSource, Style,
    SuspendedContext, TextureId, Ui,
};
use dear_imgui_winit::{HiDpiMode, WinitPlatform};
use sokol::gfx as sg;
use winit::event::WindowEvent;
use winit::window::Window;

use crate::icons;

/// The empty-window logo: `assets/icon-256.png`, decoded to raw straight-alpha RGBA at build
/// time by build.rs (no PNG decoder ships in the exe). Edge kept in sync with build.rs's
/// `LOGO_EDGE`. Also the window icon (see `app`).
pub const LOGO_EDGE: u32 = 256;
pub static LOGO_RGBA: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/logo.rgba"));

// --- sokol_imgui, as compiled by build.rs (see simgui/) ---------------------------------------

#[repr(C)]
struct SimguiAllocator {
    alloc_fn: Option<unsafe extern "C" fn(usize, *mut c_void) -> *mut c_void>,
    free_fn: Option<unsafe extern "C" fn(*mut c_void, *mut c_void)>,
    user_data: *mut c_void,
}

#[repr(C)]
struct SimguiLogger {
    func: Option<
        extern "C" fn(
            *const std::ffi::c_char,
            u32,
            u32,
            *const std::ffi::c_char,
            u32,
            *const std::ffi::c_char,
            *mut c_void,
        ),
    >,
    user_data: *mut c_void,
}

/// `simgui_desc_t`. Field order and types mirror sokol_imgui.h exactly.
#[repr(C)]
struct SimguiDesc {
    max_vertices: i32,
    color_format: sg::PixelFormat,
    depth_format: sg::PixelFormat,
    sample_count: i32,
    ini_filename: *const std::ffi::c_char,
    no_default_font: bool,
    disable_paste_override: bool,
    disable_set_mouse_cursor: bool,
    disable_windows_resize_from_edges: bool,
    write_alpha_channel: bool,
    allocator: SimguiAllocator,
    logger: SimguiLogger,
}

extern "C" {
    fn simgui_setup(desc: *const SimguiDesc);
    fn simgui_render();
    fn simgui_imtextureid(tex_view: sg::View) -> u64;
}

/// Whether `simgui_setup` has run: its sokol_gfx resources are process-wide, created by the first
/// window and shared by every later one.
static SIMGUI_UP: AtomicBool = AtomicBool::new(false);

/// A texture the UI draws from, owned here: the sokol image and the view its `TextureId` names.
/// Both must outlive every frame that references the id, so they are only ever replaced wholesale.
struct UiTexture {
    image: sg::Image,
    view: sg::View,
}

impl UiTexture {
    fn destroy(self) {
        if sg::isvalid() {
            sg::destroy_view(self.view);
            sg::destroy_image(self.image);
        }
    }
}

pub struct Imgui {
    ctx: SuspendedContext,
    platform: WinitPlatform,
    window: Arc<Window>,
    /// The font-atlas claim `dear-imgui-rs` requires of a managed renderer (see [`Imgui::new`]).
    _consumer: SynchronousRendererConsumer,
    /// The icon atlas.
    icon: Option<UiTexture>,
    icon_id: TextureId,
    /// Physical edge the atlas was last rastered at, so [`Imgui::refresh_icons`] can tell whether a
    /// DPI or stylesheet change actually moved it.
    icon_built_px: f32,
    /// ...and the per-icon scales it was rastered with, which a hot reload can move on their own.
    /// Zeroes until the first build, and zero is never a legal scale, so the first check builds.
    icon_built_scales: [f32; icons::COUNT],
    /// The empty-window logo, built lazily by [`Imgui::logo`] — an image launch never uploads it.
    logo: Option<UiTexture>,
    logo_id: TextureId,
    /// Set once [`Imgui::logo`] has tried, success or not — a failed build is not retried (and
    /// not re-logged) every empty-state frame.
    logo_built: bool,
    dpi: u32,
    /// ImGui's factory style, captured at creation *before* [`crate::ui::theme`] overwrites it.
    /// The settings window is drawn with this — see [`FormStyle`].
    stock: sys::ImGuiStyle,
    /// ImGui's ownership booleans, cached after every event and frame so the shell can route
    /// input without reaching into the context: does a widget own the pointer, do the keys belong
    /// to ImGui, is a text field being typed into.
    want_mouse: bool,
    want_keyboard: bool,
    want_text: bool,
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
    /// and out of this module.
    pub fn style_mut(&mut self) -> &mut Style {
        // SAFETY: layout-compatible by the wrapper's own const assertions.
        unsafe { &mut *(&mut self.0 as *mut sys::ImGuiStyle as *mut Style) }
    }

    /// Install it until the guard drops.
    ///
    /// Assigning `ImGuiStyle` mid-frame is exactly how ImGui implements `PushStyleVar` itself: the
    /// struct is a POD, read at widget-submission time, so windows built before and after the guard
    /// are untouched.
    #[must_use]
    pub fn push(self) -> StyleGuard {
        // SAFETY: called from inside a frame, where the context is current.
        unsafe {
            let live = sys::igGetStyle();
            let saved = *live;
            *live = self.0;
            StyleGuard(saved)
        }
    }
}

/// Restores the style [`FormStyle::push`] replaced.
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
pub fn seed_colors(style: &mut Style, dark: bool) {
    // SAFETY: `Style` is `#[repr(transparent)]` over `ImGuiStyle` (the crate asserts the layout), and
    // `igStyleColors*` only writes the color array — the metrics and font fields are untouched.
    let raw = style as *mut Style as *mut sys::ImGuiStyle;
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
    place(
        p,
        sys::ImVec2_c { x: 0.0, y: 0.0 },
        sys::ImGuiCond_Appearing,
    );
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
    /// Build the context, attach the winit platform to `window`, and bind sokol_imgui to it (see
    /// the module notes on how the two share it). Must run after `sg_setup`. Errors are strings
    /// for the caller to show.
    pub fn new(window: Arc<Window>, dpi: u32) -> Result<Self, String> {
        let mut ctx = SuspendedContext::create();
        let window_for_platform = Arc::clone(&window);
        let built = ctx.try_with_active(|c| -> Result<_, String> {
            // No imgui.ini: fire has no dockspaces or user-arranged windows to persist, and a
            // settings file that rewrites itself on a timer would break the "an idle window costs
            // ~0" invariant.
            c.set_ini_filename(None::<PathBuf>)
                .map_err(|e| format!("set_ini_filename: {e}"))?;

            // The system UI font, to match the rest of the desktop. Registering the font costs
            // ~0.4 ms and bakes no glyphs (1.92 rasterizes on first draw); if it is somehow
            // missing, ImGui's built-in font stands in rather than the app failing to start.
            if let Some(ttf) = crate::platform::ui_font_path().and_then(|p| std::fs::read(p).ok()) {
                // SAFETY: `ttf` is a complete font file read from the OS's own font directory,
                // and it is not touched between here and `add_font`, which copies it. Dynamic
                // sizing: glyphs are sized per frame from the style, not from the source.
                let source = unsafe { FontSource::ttf_data(&ttf) };
                c.font_atlas().add_font(&[source]);
            }

            let mut platform = WinitPlatform::new(c).map_err(|e| format!("winit backend: {e}"))?;
            // Physical pixels throughout (see the module notes on DPI).
            platform
                .set_hidpi_mode(HiDpiMode::Locked(1.0))
                .map_err(|e| format!("winit backend: {e}"))?;
            platform
                .attach_window(window_for_platform, HiDpiMode::Locked(1.0), c)
                .map_err(|e| format!("winit backend: {e}"))?;

            // Snapshot the factory style *now*, before `ui::theme::apply` runs over it — this is
            // the only moment it exists. SAFETY: this context is current inside the closure.
            let stock = unsafe { *sys::igGetStyle() };

            // sokol_imgui, once per process: its sokol_gfx resources, and a context we do not keep
            // (module notes).
            if !SIMGUI_UP.swap(true, Ordering::AcqRel) {
                simgui_setup_once(c);
            }
            // What `simgui_setup` told *its* context: the renderer takes ImGui's textures (fonts
            // are built through the texture protocol) and honours `vtx_offset`.
            let io = c.io_mut();
            io.set_backend_flags(
                io.backend_flags()
                    | BackendFlags::RENDERER_HAS_TEXTURES
                    | BackendFlags::RENDERER_HAS_VTX_OFFSET,
            );
            // `dear-imgui-rs` will not open a frame on a `RENDERER_HAS_TEXTURES` context unless a
            // renderer has claimed the font atlas through its own API. The claim is all that is
            // used of it: sokol_imgui does the actual uploads, and `ctx.render()` is never called.
            let consumer = c
                .create_synchronous_renderer_consumer()
                .map_err(|e| format!("renderer consumer: {e}"))?;
            Ok((platform, stock, consumer))
        });
        let (platform, stock, consumer) = match built {
            Ok(v) => v,
            Err(e) => return Err(format!("ImGui: {e}")),
        };

        let mut me = Imgui {
            ctx,
            platform,
            window,
            _consumer: consumer,
            icon: None,
            icon_id: TextureId::new(0),
            icon_built_px: 0.0,
            icon_built_scales: [0.0; icons::COUNT],
            logo: None,
            logo_id: TextureId::new(0),
            logo_built: false,
            dpi: dpi.max(96),
            stock,
            want_mouse: false,
            want_keyboard: false,
            want_text: false,
        };
        me.refresh_icons();
        Ok(me)
    }

    /// Run `f` with this window's context current.
    fn with<R>(&mut self, f: impl FnOnce(&mut Context) -> R) -> R {
        self.ctx.with_active_or_panic(f)
    }

    /// The settings window's base style: ImGui's factory geometry, scaled for the monitor, carrying
    /// our font — for [`crate::ui::theme::form`] to paint and [`FormStyle::push`] to install.
    ///
    /// Composed per call rather than cached: it is a ~1 KB POD copy plus two library calls, against a
    /// frame that is only drawn when something happened. Caching it would mean invalidating it on DPI
    /// *and* theme changes, which is more state than it saves.
    pub fn form_style(&mut self, dark: bool) -> FormStyle {
        let mut s = self.stock;
        let dpi = self.dpi;
        self.with(|c| {
            // Factory geometry, scaled to the monitor. `ui::theme::form` then overrides the
            // metrics it cares about; the rest (cell padding, separator-text padding, …) stay
            // correctly scaled. Seeds every color, including the dozens the theme doesn't name.
            unsafe {
                sys::ImGuiStyle_ScaleAllSizes(&mut s, dpi as f32 / 96.0);
                if dark {
                    sys::igStyleColorsDark(&mut s);
                } else {
                    sys::igStyleColorsLight(&mut s);
                }
            }
            // The font is ours (the system UI font); its size and DPI scale come from the live
            // style so the settings window renders text exactly like the rest of the app.
            let live = c.style();
            s.FontSizeBase = live.font_size_base();
            s.FontScaleMain = live.font_scale_main();
            s.FontScaleDpi = live.font_scale_dpi();
        });
        FormStyle(s)
    }

    /// Physical icon edge for the current DPI. The *logical* edge is a stylesheet value
    /// (`[font] icon_size` in `ui/theme.toml`), so this moves on a hot reload as well as on a DPI
    /// change — which is what [`Imgui::refresh_icons`] is for.
    pub fn icon_px(&self) -> f32 {
        (crate::ui::theme::current().font.icon_size * self.dpi as f32 / 96.0).round()
    }

    /// The stylesheet's per-icon shrink factors, in the order [`icons::atlas`] indexes them. Also a
    /// hot-reloadable value, and one that moves *without* [`Imgui::icon_px`] moving — which is why
    /// [`Imgui::refresh_icons`] compares it too.
    fn icon_scales(&self) -> [f32; icons::COUNT] {
        let theme = crate::ui::theme::current();
        std::array::from_fn(|i| theme.icon_scale(icons::ALL[i]))
    }

    /// Edit the live style (the chrome's) — [`crate::ui::theme::apply`] goes through here.
    pub fn restyle(&mut self, f: impl FnOnce(&mut Style)) {
        self.with(|c| f(c.style_mut()));
    }

    /// Adopt a new DPI. ImGui 1.92 re-bakes *glyphs* lazily, so there is no font atlas to rebuild
    /// and nothing else to do here; the icon atlas is a real raster and is the caller's next call
    /// ([`Imgui::refresh_icons`]), and the style (including `font_scale_dpi`) goes through
    /// [`crate::ui::theme::apply`] — the one place metrics are decided.
    pub fn set_dpi(&mut self, dpi: u32) {
        self.dpi = dpi.max(96);
    }

    /// Re-raster the icon atlas if anything it was baked from has moved — the physical icon size (a
    /// DPI change or a stylesheet edit) or the per-icon scales (a stylesheet edit alone, which
    /// leaves the size untouched). Cheap no-op when nothing has, so the restyle path can call it
    /// unconditionally.
    pub fn refresh_icons(&mut self) {
        if self.icon_px() != self.icon_built_px || self.icon_scales() != self.icon_built_scales {
            self.rebuild_icons();
        }
    }

    fn rebuild_icons(&mut self) {
        let px = self.icon_px();
        let n = px as usize;
        let scales = self.icon_scales();
        let (pixels, w) = icons::atlas(n, &scales);
        let old = self.icon.take();
        let (texture, id) = register_texture(old, &pixels, w as u32, n as u32, c"icon atlas");
        self.icon = texture;
        self.icon_id = id;
        self.icon_built_px = px;
        self.icon_built_scales = scales;
    }

    /// The empty-window logo texture, built on first call — which the shell only makes on an
    /// empty-state frame, so a launch straight into an image never pays the upload on its
    /// time-to-first-photon path. Zero id when creation failed; the card then draws text alone.
    pub fn logo(&mut self) -> TextureId {
        if !self.logo_built {
            self.logo_built = true;
            let (texture, id) = register_texture(None, LOGO_RGBA, LOGO_EDGE, LOGO_EDGE, c"logo");
            self.logo = texture;
            self.logo_id = id;
        }
        self.logo_id
    }

    /// Feed a window event to ImGui. The ownership booleans are refreshed afterwards for the
    /// shell's routing.
    pub fn handle_event(&mut self, event: &WindowEvent) {
        let (platform, window) = (&mut self.platform, &self.window);
        let want = self.ctx.with_active_or_panic(|c| {
            if let Err(e) = platform.handle_window_event(c, window, event) {
                eprintln!("fire: ImGui platform rejected an event: {e}");
            }
            wants(c)
        });
        (self.want_mouse, self.want_keyboard, self.want_text) = want;
    }

    /// True when a widget (not the image) owns the pointer — the toolbar, status bar, a popup.
    pub fn wants_mouse(&self) -> bool {
        self.want_mouse
    }

    /// True when ImGui wants the keys — a focused text field, or an open popup.
    pub fn wants_keyboard(&self) -> bool {
        self.want_keyboard
    }

    /// True while a text field is being edited. The *only* thing in fire that needs a repaint with
    /// no input behind it (the caret blink), so the shell arms a timer on it — and kills it the
    /// moment this goes false, or an idle window would stop being free.
    pub fn wants_text_input(&self) -> bool {
        self.want_text
    }

    /// Build and render one UI frame into the current sokol_gfx pass (the surface's contract).
    /// Returns what `build` produced, or `None` if it panicked (logged; the frame is still closed
    /// and whatever was built is drawn).
    pub fn frame<R>(&mut self, build: impl FnOnce(&Ui, TextureId) -> R) -> Option<R> {
        let icon_id = self.icon_id;
        let Self {
            ctx,
            platform,
            window,
            ..
        } = self;
        let (out, want) = ctx.with_active_or_panic(|c| {
            if let Err(e) = platform.prepare_frame(c, window) {
                eprintln!("fire: ImGui platform prepare_frame failed: {e}");
            }
            let ui: &Ui = c.frame();
            let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| build(ui, icon_id)));
            // The OS cursor and IME state follow the UI that was just built.
            if let Err(e) = platform.prepare_render(ui, window) {
                eprintln!("fire: ImGui platform prepare_render failed: {e}");
            }
            let want = wants(c);
            // Close the frame and draw it — even after a panic in `build`, so the context is never
            // left mid-frame. SAFETY: a frame is open on the current context; a sokol_gfx pass is
            // open (the surface's contract).
            unsafe { simgui_render() };
            (out, want)
        });
        (self.want_mouse, self.want_keyboard, self.want_text) = want;
        match out {
            Ok(r) => Some(r),
            Err(_) => {
                eprintln!("fire: recovered from a panic while building the UI");
                None
            }
        }
    }
}

impl Drop for Imgui {
    fn drop(&mut self) {
        // Release our textures while sokol_gfx is still up, and the platform backend with the
        // context current so its teardown is honored; the context itself goes with the struct.
        // sokol_imgui's own resources go with the process: `simgui_shutdown` would destroy the
        // *current* ImGui context, which is one of ours.
        if let Some(t) = self.icon.take() {
            t.destroy();
        }
        if let Some(t) = self.logo.take() {
            t.destroy();
        }
        let platform = &mut self.platform;
        let _ = self.ctx.try_with_active(|c| -> Result<(), ()> {
            let _ = platform.shutdown(c);
            Ok(())
        });
    }
}

/// ImGui's input-ownership booleans, read off the current context.
fn wants(c: &Context) -> (bool, bool, bool) {
    let io = c.io();
    (
        io.want_capture_mouse(),
        io.want_capture_keyboard(),
        io.want_text_input(),
    )
}

/// `simgui_setup`, run once with the first window's context `c` current: sokol_imgui creates its
/// sokol_gfx resources and a context of its own, which is destroyed on the spot — sokol_imgui
/// holds no reference to it, and every window's context carries the backend flags it would have
/// set (see [`Imgui::new`]).
fn simgui_setup_once(c: &mut Context) {
    let desc = SimguiDesc {
        max_vertices: 0,
        color_format: sg::PixelFormat::Default,
        depth_format: sg::PixelFormat::Default,
        sample_count: 0,
        ini_filename: std::ptr::null(),
        // Our font is registered by the caller; the built-in ProggyClean would only cost an atlas.
        no_default_font: true,
        disable_paste_override: false,
        // The OS cursor is the winit backend's job (`prepare_render`).
        disable_set_mouse_cursor: true,
        disable_windows_resize_from_edges: false,
        write_alpha_channel: false,
        allocator: SimguiAllocator {
            alloc_fn: None,
            free_fn: None,
            user_data: std::ptr::null_mut(),
        },
        logger: SimguiLogger {
            func: Some(sokol::log::slog_func),
            user_data: std::ptr::null_mut(),
        },
    };
    // SAFETY: `desc` is fully initialized; sokol_gfx is set up (the caller's contract). The
    // context sokol_imgui creates is destroyed here, and `c` made current again, before anything
    // else runs — sokol_imgui holds no reference to it.
    unsafe {
        simgui_setup(&desc);
        let theirs = sys::igGetCurrentContext();
        if theirs != c.as_raw() {
            sys::igDestroyContext(theirs);
            sys::igSetCurrentContext(c.as_raw());
        }
    }
}

/// Upload an RGBA8 texture and name it for ImGui (destroying `old` first). On failure the missing
/// pieces come back `None` / zero id (logged) and the caller draws without the texture rather than
/// the app failing. Straight sRGB bytes in a UNORM image: the UI pass writes through the UNORM
/// swapchain view, so nothing is decoded on the way through.
fn register_texture(
    old: Option<UiTexture>,
    pixels: &[u8],
    w: u32,
    h: u32,
    what: &std::ffi::CStr,
) -> (Option<UiTexture>, TextureId) {
    if let Some(old) = old {
        old.destroy();
    }
    let mut desc = sg::ImageDesc::new();
    desc._type = sg::ImageType::Dim2;
    desc.width = w as i32;
    desc.height = h as i32;
    desc.num_mipmaps = 1;
    desc.pixel_format = sg::PixelFormat::Rgba8;
    desc.data.mip_levels[0] = sg::slice_as_range(pixels);
    desc.label = what.as_ptr();
    let image = sg::make_image(&desc);
    let mut vd = sg::ViewDesc::new();
    vd.texture.image = image;
    vd.label = what.as_ptr();
    let view = sg::make_view(&vd);
    if sg::query_image_state(image) != sg::ResourceState::Valid
        || sg::query_view_state(view) != sg::ResourceState::Valid
    {
        eprintln!(
            "fire: {} could not be uploaded for the UI",
            what.to_string_lossy()
        );
        sg::destroy_view(view);
        sg::destroy_image(image);
        return (None, TextureId::new(0));
    }
    // SAFETY: a plain id computation over two valid handles.
    let id = unsafe { simgui_imtextureid(view) };
    (Some(UiTexture { image, view }), TextureId::new(id))
}
