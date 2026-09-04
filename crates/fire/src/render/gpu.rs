//! GPU viewport: the decoded image drawn through sokol_gfx into the window's swapchain (the
//! shell's — see [`crate::render::d3d11`]), into a *sub-rect* of the window (`Viewer::image_rect`)
//! with the ImGui chrome over the rest.
//! The image lives as a GPU texture with a full mip chain, and pan/zoom/exposure/channel/tonemap
//! are just uniform values, so each frame is one textured fullscreen triangle: the per-frame CPU
//! cost is a 128-byte [`Params`] upload + a draw call, and the GPU does the sampling and the whole
//! color pipeline.
//!
//! Panning changes a transform and the GPU re-samples the texture rather than re-running a
//! per-pixel pipeline on the CPU. Presentation is the swapchain's, vsync-paced, and event-driven:
//! a frame is drawn only when the shell asks for one.
//!
//! Color: 8-bit sources upload as `SRGB8A8` (hardware sRGB→linear on sample), float sources are
//! already linear, 16-bit unorm is sRGB-decoded in the shader. The pixel shader works in linear
//! light and sRGB-encodes its output itself (see `shader.hlsl`): the swapchain is drawn through a
//! UNORM view, which is also what lets Dear ImGui's already-sRGB colors land in the same pass
//! untouched.
//!
//! Two structs, by lifetime. [`Gpu`] is the process's device and pipeline state: the D3D11 device,
//! sokol_gfx on it, the shader, the pipeline, the samplers and a placeholder texture — built once,
//! on the bring-up thread. [`GpuSurface`] is the window's render state: its swapchain, the image
//! texture and the same view/session/gesture state the earlier
//! shells carried, grouped the same way ([`ImageContent`] and [`AnimState`] are replaced wholesale
//! on every adopt; [`SessionPrefs`] deliberately outlives the image so a chosen backdrop survives
//! folder navigation; [`GestureState`] lives only between a button-down and its up). The view core
//! (`origin`/`viewport`/`view`/`display`/`flipbook`) stays flat on the surface — nearly every
//! method touches it, and its math already lives in [`crate::render::view`].
//!
//! The mip chain is the app's to supply (sokol_gfx has no `GenerateMips` and its rules rule out
//! building one in place); [`crate::render::mips`] builds it on the decode worker and the adopt
//! uploads every level in one `sg_make_image`.

use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use fire_decode::{AnimationFrame, DecodedImage, PixelFormat};
use sokol::gfx as sg;
use winit::window::Window;

use crate::render::d3d11;
use crate::render::mips;
use crate::render::view::{
    Background, Channel, DisplayState, Tonemap, ViewState, Viewport, ZoomDetent,
};

/// Scrubby-zoom sensitivity: an RMB vertical drag multiplies zoom by `exp(dy * this)` per pixel
/// (~2.7× per 100 px). Exponential-in-pixels so the gesture feels uniform across the zoom range;
/// drag down (dy > 0) zooms in, up zooms out.
const ZOOM_DRAG_SENSITIVITY: f32 = 0.01;

/// How far (surface px) the cursor may move during an RMB press before it counts as a zoom-drag
/// rather than a right-*click*. Below this the gesture opens the context menu; the tiny zoom such a
/// jiggle would apply (~exp(slop·sensitivity), a couple percent) is imperceptible.
const ZOOM_DRAG_CLICK_SLOP: f32 = 5.0;

/// Per-frame shader constants. Layout matches the HLSL `cbuffer Params` (16-byte float4
/// registers); keep the field order/padding in lockstep with `render/shader.hlsl`. 128 bytes —
/// asserted below, so this comment cannot drift from the struct, and declared to sokol_gfx as the
/// uniform block's size.
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
    /// The image sub-rect's top-left in **render-target** px — i.e. [`GpuSurface::origin`].
    ///
    /// The pixel shader's `SV_Position` is in render-target space, *not* viewport space: the
    /// viewport transform is applied before the fragment stage, so a viewport at `y = toolbar_h`
    /// still hands the shader absolute client coordinates. Without this the shader centres the
    /// image on `surf_size * 0.5` measured from the *client's* origin rather than the viewport's,
    /// and every image opens exactly `toolbar_h` px too high, its top clipped away.
    surf_origin_x: f32,
    surf_origin_y: f32,
    /// Octagon overlay: the crop factor (0 = quad, 0.5 = diamond) and how much the image outside
    /// the octagon fades toward the backdrop (0 = overlay off / no hiding).
    oct_crop: f32,
    oct_hide: f32,
}

const PARAMS_SIZE: usize = std::mem::size_of::<Params>();
const _: () = assert!(PARAMS_SIZE == 128);
const _: () = assert!(PARAMS_SIZE.is_multiple_of(16));

/// Flipbook render parameters mirrored onto the surface from the active per-path state (the
/// surface never owns durable flipbook state — the viewer does). `None` = not in flipbook mode.
#[derive(Debug, Clone, Copy)]
pub struct FlipbookParams {
    pub grid: crate::flipbook::Grid,
    pub frame_count: u32,
    pub frame_pos: f32,
    pub blend: bool,
}

/// Playback state of an animated image (animated GIF): which frame is currently uploaded to
/// the texture, over an `Arc` of the decoded image itself. Holding the `Arc` (shared with
/// `tex.current` and the decode worker) and indexing its frames in place is the point —
/// cloning the frame list here used to duplicate every composited canvas, i.e. the whole
/// animation, in RAM. `None` for a still image, the common case. The playback timer lives in
/// the viewer and advances this through [`GpuSurface::advance_frame`].
#[derive(Default)]
struct AnimState {
    img: Option<Arc<DecodedImage>>,
    index: usize,
}

impl AnimState {
    /// Adopt `img`'s animation (if it has one) and rewind to frame 0.
    fn adopt(&mut self, img: &Arc<DecodedImage>) {
        self.img = img.animation.is_some().then(|| Arc::clone(img));
        self.index = 0;
    }

    fn clear(&mut self) {
        self.img = None;
        self.index = 0;
    }

    fn frames(&self) -> &[AnimationFrame] {
        self.img
            .as_ref()
            .and_then(|i| i.animation.as_ref())
            .map_or(&[], |a| &a.frames)
    }

    /// How long the current frame should be shown, or `None` if this isn't an animation. One
    /// frame is a still: nothing to schedule.
    fn delay_ms(&self) -> Option<u32> {
        let frames = self.frames();
        (frames.len() > 1).then(|| frames[self.index].delay_ms)
    }
}

/// Append a startup-timing line to wherever `FIRE_TIMING` points: `1` prints to stderr, a path
/// appends to that file (for a release build, which has no console on Windows). A cheap no-op
/// when the variable is unset.
pub fn report_timing(line: &str) {
    let Some(sink) = std::env::var_os("FIRE_TIMING") else {
        return;
    };
    if sink == "1" {
        eprintln!("fire: {line}");
    } else {
        use std::io::Write as _;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&sink)
        {
            let _ = writeln!(f, "{line}");
        }
    }
}

/// The swapchain's format: what every window's backbuffer is created as (see [`d3d11`]) and what
/// sokol_gfx is told to expect of a swapchain pass, so the viewport and ImGui pipelines match it.
const SWAPCHAIN_FORMAT: sg::PixelFormat = sg::PixelFormat::Rgba8;

/// The process's GPU: the device sokol_gfx runs on and the pipeline state on it — the viewport
/// shader and pipeline, the two samplers and a placeholder texture. Built once, on the bring-up
/// thread ([`Gpu::start`]), and shared by every window's surface.
pub struct Gpu {
    /// The D3D11 device; every window's swapchain is created on it.
    device: d3d11::Device,
    _shader: sg::Shader,
    pipeline: sg::Pipeline,
    samp_aniso: sg::Sampler,
    samp_point: sg::Sampler,
    /// A 1×1 texture bound while no image is loaded, so the bindings are always complete (the
    /// shader never reads it: `has_image == 0` returns the clear color).
    _placeholder: sg::Image,
    placeholder_view: sg::View,
    /// Whether 32-bit float textures may be linearly filtered on this device. Without it a
    /// float32 source is uploaded as float16 instead.
    float32_filterable: bool,
    /// Whether 16-bit unorm textures exist on this device. Without it a 16-bit source is uploaded
    /// as float16 (still sRGB-decoded in the shader).
    norm16: bool,
}

impl Gpu {
    /// Start bringing the GPU up on a worker thread, right now — the device, `sg_setup` and the
    /// pipeline state need no window, and together they are the longest single item on the
    /// launch path, so they run alongside the window creation, the ImGui setup and the decode
    /// instead of after them. The first window joins the handle (`Fire::gpu`).
    pub fn start() -> std::thread::JoinHandle<Result<Gpu, String>> {
        std::thread::Builder::new()
            .name("fire-gpu-init".into())
            .spawn(Self::bring_up)
            .expect("the GPU bring-up thread must start")
    }

    /// Bring the GPU up: the device, sokol_gfx on it, then the pipeline state. Errors are strings
    /// for the caller to show: this runs at startup in a process that may have no console.
    ///
    /// sokol_gfx is a process-wide singleton and not thread-safe, but it has no thread affinity:
    /// set up here and used from the main thread after the join, which is the synchronization.
    pub fn bring_up() -> Result<Gpu, String> {
        let t = Instant::now();
        let device = d3d11::Device::create()?;
        report_timing(&format!(
            "d3d11 device — {:.2} ms",
            t.elapsed().as_secs_f64() * 1e3
        ));

        let t = Instant::now();
        let mut desc = sg::Desc::new();
        desc.environment.defaults = sg::EnvironmentDefaults {
            color_format: SWAPCHAIN_FORMAT,
            depth_format: sg::PixelFormat::None,
            sample_count: 1,
        };
        desc.environment.d3d11 = sg::D3d11Environment {
            device: device.raw_device(),
            device_context: device.raw_context(),
        };
        desc.logger = sg::Logger {
            func: Some(sokol::log::slog_func),
            user_data: std::ptr::null_mut(),
        };
        sg::setup(&desc);
        if !sg::isvalid() {
            return Err("sokol_gfx could not be set up on the D3D11 device".into());
        }
        report_timing(&format!(
            "sg setup — {:.2} ms",
            t.elapsed().as_secs_f64() * 1e3
        ));

        Self::pipeline_state(device)
    }

    /// A flip-model swapchain on `window`'s client, for a [`GpuSurface`].
    fn create_swapchain(
        &self,
        window: &Window,
        width: u32,
        height: u32,
    ) -> Result<d3d11::Swapchain, String> {
        d3d11::Swapchain::new(&self.device, window, width, height)
    }

    /// Build the pipeline state on the (set-up) sokol_gfx.
    fn pipeline_state(device: d3d11::Device) -> Result<Gpu, String> {
        let t = Instant::now();
        let shader = make_shader()?;

        let mut pd = sg::PipelineDesc::new();
        pd.shader = shader;
        pd.primitive_type = sg::PrimitiveType::Triangles;
        pd.cull_mode = sg::CullMode::None;
        pd.label = c"fire viewport".as_ptr();
        let pipeline = sg::make_pipeline(&pd);
        if sg::query_pipeline_state(pipeline) != sg::ResourceState::Valid {
            return Err("the viewport pipeline could not be created".into());
        }

        // Two samplers: anisotropic+mips for minify, point for crisp magnify/1:1. Both clamp at
        // edges.
        let mut sd = sg::SamplerDesc::new();
        sd.min_filter = sg::Filter::Linear;
        sd.mag_filter = sg::Filter::Linear;
        sd.mipmap_filter = sg::Filter::Linear;
        sd.wrap_u = sg::Wrap::ClampToEdge;
        sd.wrap_v = sg::Wrap::ClampToEdge;
        sd.wrap_w = sg::Wrap::ClampToEdge;
        sd.max_anisotropy = 8;
        sd.label = c"fire aniso".as_ptr();
        let samp_aniso = sg::make_sampler(&sd);
        let mut sd = sg::SamplerDesc::new();
        sd.min_filter = sg::Filter::Nearest;
        sd.mag_filter = sg::Filter::Nearest;
        sd.mipmap_filter = sg::Filter::Nearest;
        sd.wrap_u = sg::Wrap::ClampToEdge;
        sd.wrap_v = sg::Wrap::ClampToEdge;
        sd.wrap_w = sg::Wrap::ClampToEdge;
        sd.label = c"fire point".as_ptr();
        let samp_point = sg::make_sampler(&sd);
        for s in [samp_aniso, samp_point] {
            if sg::query_sampler_state(s) != sg::ResourceState::Valid {
                return Err("the viewport samplers could not be created".into());
            }
        }

        let texel = [0u8, 0, 0, 255];
        let mut id = sg::ImageDesc::new();
        id._type = sg::ImageType::Dim2;
        id.width = 1;
        id.height = 1;
        id.num_mipmaps = 1;
        id.pixel_format = sg::PixelFormat::Srgb8a8;
        id.data.mip_levels[0] = sg::slice_as_range(&texel);
        id.label = c"fire placeholder".as_ptr();
        let placeholder = sg::make_image(&id);
        let mut vd = sg::ViewDesc::new();
        vd.texture.image = placeholder;
        vd.label = c"fire placeholder".as_ptr();
        let placeholder_view = sg::make_view(&vd);
        if sg::query_image_state(placeholder) != sg::ResourceState::Valid
            || sg::query_view_state(placeholder_view) != sg::ResourceState::Valid
        {
            return Err("the placeholder texture could not be created".into());
        }

        let norm16 = sg::query_pixelformat(sg::PixelFormat::Rgba16).sample;
        let float32_filterable = sg::query_pixelformat(sg::PixelFormat::Rgba32f).filter;

        report_timing(&format!(
            "gpu pipeline — {:.2} ms ({:?})",
            t.elapsed().as_secs_f64() * 1e3,
            sg::query_backend()
        ));

        Ok(Gpu {
            device,
            _shader: shader,
            pipeline,
            samp_aniso,
            samp_point,
            _placeholder: placeholder,
            placeholder_view,
            float32_filterable,
            norm16,
        })
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        // Only while sokol_gfx is still up; the shell shuts it down after the viewer is gone.
        if sg::isvalid() {
            sg::destroy_view(self.placeholder_view);
            sg::destroy_image(self._placeholder);
            sg::destroy_sampler(self.samp_aniso);
            sg::destroy_sampler(self.samp_point);
            sg::destroy_pipeline(self.pipeline);
            sg::destroy_shader(self._shader);
        }
    }
}

/// The viewport shader, from the DXBC `fxc` precompiled at build time (see `build.rs`). The
/// description is the reflection sokol_gfx cannot do for us: the one 128-byte uniform block at
/// `b0` (fragment stage), the texture at `t0`, the anisotropic sampler at `s0` and the point
/// sampler at `s1`, and which sampler pairs with the texture.
#[cfg(windows)]
fn make_shader() -> Result<sg::Shader, String> {
    static VS_DXBC: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/vs_main.dxbc"));
    static PS_DXBC: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/ps_main.dxbc"));

    let mut d = sg::ShaderDesc::new();
    d.vertex_func.bytecode = sg::slice_as_range(VS_DXBC);
    d.fragment_func.bytecode = sg::slice_as_range(PS_DXBC);
    d.uniform_blocks[0] = sg::ShaderUniformBlock {
        stage: sg::ShaderStage::Fragment,
        size: PARAMS_SIZE as u32,
        hlsl_register_b_n: 0,
        ..sg::ShaderUniformBlock::new()
    };
    d.views[0].texture = sg::ShaderTextureView {
        stage: sg::ShaderStage::Fragment,
        image_type: sg::ImageType::Dim2,
        sample_type: sg::ImageSampleType::Float,
        multisampled: false,
        hlsl_register_t_n: 0,
        ..sg::ShaderTextureView::new()
    };
    d.samplers[0] = sg::ShaderSampler {
        stage: sg::ShaderStage::Fragment,
        sampler_type: sg::SamplerType::Filtering,
        hlsl_register_s_n: 0,
        ..sg::ShaderSampler::new()
    };
    d.samplers[1] = sg::ShaderSampler {
        stage: sg::ShaderStage::Fragment,
        sampler_type: sg::SamplerType::Nonfiltering,
        hlsl_register_s_n: 1,
        ..sg::ShaderSampler::new()
    };
    d.texture_sampler_pairs[0] = sg::ShaderTextureSamplerPair {
        stage: sg::ShaderStage::Fragment,
        view_slot: 0,
        sampler_slot: 0,
        ..sg::ShaderTextureSamplerPair::new()
    };
    d.texture_sampler_pairs[1] = sg::ShaderTextureSamplerPair {
        stage: sg::ShaderStage::Fragment,
        view_slot: 0,
        sampler_slot: 1,
        ..sg::ShaderTextureSamplerPair::new()
    };
    d.label = c"fire viewport".as_ptr();
    let shader = sg::make_shader(&d);
    if sg::query_shader_state(shader) != sg::ResourceState::Valid {
        return Err("the viewport shader could not be created".into());
    }
    Ok(shader)
}

/// The Metal (MSL) twin of the viewport shader is not compiled yet; this prototype measures the
/// Windows shell.
#[cfg(not(windows))]
fn make_shader() -> Result<sg::Shader, String> {
    Err("the viewport shader is only built for D3D11 in this prototype".into())
}

/// A pixel conversion applied to every level when the device lacks the source format.
type Convert = fn(&[u8]) -> Vec<u8>;

/// The flipbook cells one frame samples: the sheet size (texels), the two cell origins to blend
/// between, the blend factor, and the mip-LOD clamp that stops a minified sample bleeding across
/// a cell boundary. `None` outside flipbook mode.
type FlipbookCells = Option<((u32, u32), (f32, f32), (f32, f32), f32, f32)>;

/// What the GPU currently holds for the displayed image: the texture, how to sample it, and the
/// decoded source it came from. All are replaced together on every adopt and cleared together
/// when the image goes away.
#[derive(Default)]
struct ImageContent {
    /// Current image texture + its sampling view (None until the first image lands).
    image: Option<sg::Image>,
    view: Option<sg::View>,
    /// 1 if the texture samples already-linear (8-bit sRGB / float), 0 if the shader must
    /// sRGB-decode (16-bit unorm).
    linear_sample: i32,
    /// The displayed image — retained for the pixel inspector (#16) and for re-fit on resize.
    /// For an animated source this holds frame 0 (dimensions/format/metadata are frame-invariant);
    /// the frames themselves live in [`AnimState`]. Held behind an `Arc` because the decode worker
    /// keeps a clone alive to run flipbook detection *after* the image has been posted for display
    /// (so detection never delays time-to-first-pixel); both sides only ever read it.
    current: Option<Arc<DecodedImage>>,
}

impl ImageContent {
    /// Release the GPU texture, if any.
    fn release(&mut self) {
        if let Some(v) = self.view.take() {
            sg::destroy_view(v);
        }
        if let Some(i) = self.image.take() {
            sg::destroy_image(i);
        }
    }
}

/// Preferences and toggles that belong to the *session* rather than to any one image: they are
/// seeded from the config, may be changed from the toolbar or the settings window, and
/// deliberately survive navigating to the next image (which is the behaviour that makes flipping
/// through a folder with a chosen backdrop and outline usable at all).
struct SessionPrefs {
    /// No-image backdrop (empty window), packed sRGB and its linear form (the shader encodes).
    clear: u32,
    clear_lin: [f32; 4],

    /// Viewport backdrop while an image is shown; defaults per-image (opaque → black, alpha →
    /// checker) and is overridden by the toolbar's background buttons.
    background: Background,
    /// The user's explicit backdrop pick, if any. Once set via the toolbar it sticks for the rest
    /// of the session (every later image adopts it instead of its per-type default); `None` until
    /// the user chooses, so each image still gets its natural default before the first override.
    background_override: Option<Background>,

    /// Draw a 1px outline around the image boundary (toolbar toggle). Starts on unless the
    /// `default-outline` config key says otherwise; the pick then persists across navigation for
    /// the rest of the session, like the backdrop.
    outline: bool,

    /// The octagon overlay (toolbar toggle + its options window). Session-global like the outline:
    /// it persists across navigation. The line render is the UI's (an ImGui draw list); the shader
    /// only consumes `crop`/`hide` for the hide-outside fade.
    octagon: crate::octagon::OctagonState,

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
    /// The zoom levels every zoom input detents on, as zoom factors (the config stores
    /// percentages), and how far past one the drag must travel to break out, in drag px. Both from
    /// the config (`zoom-snap-levels` / `zoom-snap`) via [`GpuSurface::set_zoom_snapping`]; an
    /// empty ladder or a zero distance is snapping switched off.
    zoom_snaps: Vec<f32>,
    zoom_snap_px: f32,
}

/// The in-flight mouse gesture: where the cursor is, and which drag (if either) owns it.
///
/// Grouped because these fields are only ever meaningful together and only between a button-down
/// and its matching up — unlike the view state they act on, which outlives every gesture. Holds
/// no config: the detent *ladder* is a session preference, and only the detent's position within
/// a single drag lives here.
#[derive(Default)]
struct GestureState {
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
    /// Detent state for the active zoom-drag: the snap the zoom is currently resting on, if any.
    zoom_detent: ZoomDetent,
}

impl GestureState {
    /// Begin an RMB zoom-drag, pivoting on the current cursor (the press point).
    fn begin_zoom_drag(&mut self) {
        self.zoom_dragging = true;
        self.zoom_anchor = self.cursor;
        self.zoom_last_y = self.cursor.1;
        self.zoom_dragged = false;
        self.zoom_detent.reset();
    }

    /// End an RMB gesture. Returns `true` if it was an actual zoom-drag (the cursor moved past
    /// [`ZOOM_DRAG_CLICK_SLOP`]); `false` if it was effectively a right-click.
    fn end_zoom_drag(&mut self) -> bool {
        self.zoom_dragging = false;
        self.zoom_dragged
    }

    /// A pan or zoom drag is in progress, i.e. the image owns the mouse until the button comes up.
    fn is_mouse_captured(&self) -> bool {
        self.dragging || self.zoom_dragging
    }
}

/// What [`GpuSurface::render_frame`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presented {
    /// No frame was drawn: the window has no area, or its backbuffer is unavailable.
    Skipped,
    /// A frame was presented. `waited` says whether the present blocked on the display —
    /// normally it does (sync interval 1, two buffers), and that wait is what playback is paced
    /// on; a present that did not wait (the swapchain was not yet full, or nobody is looking)
    /// must not be, or pacing on it would spin.
    Yes { waited: bool },
}

/// The window's GPU render state: its image texture on the shared [`Gpu`], plus the same
/// pan/zoom/fit and channel/exposure/tonemap state the earlier shells carried, so the viewer and
/// chrome drive it through an identical API.
pub struct GpuSurface {
    gpu: Rc<Gpu>,
    window: Arc<Window>,
    /// The window's swapchain, sized to its client.
    swapchain: d3d11::Swapchain,
    /// The image texture + the two samplers, ready to apply. Rebuilt whenever the texture changes.
    bindings: sg::Bindings,

    /// The image sub-rect's origin within the client (the chrome occupies the rest).
    origin: (f32, f32),
    /// What the non-image parts of the frame are cleared to before the UI draws over them, as
    /// sRGB (the theme's value; the swapchain is written unencoded).
    chrome_clear: [f32; 4],

    /// The displayed image's GPU residency.
    tex: ImageContent,

    /// This window's current decode generation; a `DecodeDone` older than this is stale. Values
    /// come from the process-wide counter (see [`crate::decode_pool::fresh_generation`]).
    generation: u64,

    anim: AnimState,

    /// Preferences and toggles that outlive the displayed image.
    prefs: SessionPrefs,

    viewport: Viewport,
    view: ViewState,
    /// Active flipbook render parameters, mirrored from the viewer's per-path state. `Some`
    /// makes pan/zoom/fit operate on the frame rect and the shader sample a single cell. The
    /// surface never persists this — it is (re)applied on every adopt via [`Self::set_flipbook`].
    flipbook: Option<FlipbookParams>,
    display: DisplayState,
    gesture: GestureState,
}

impl GpuSurface {
    /// Create the window's swapchain and build the surface state for its `width`×`height`
    /// client (the image is drawn into a sub-rect of it — see [`Self::set_image_rect`] — with
    /// the chrome over the remainder).
    pub fn new(
        gpu: Rc<Gpu>,
        window: Arc<Window>,
        width: u32,
        height: u32,
        fit_upscale: bool,
    ) -> Result<Self, String> {
        let swapchain = gpu.create_swapchain(&window, width, height)?;
        let bindings = make_bindings(&gpu, gpu.placeholder_view);
        Ok(Self {
            gpu,
            window,
            swapchain,
            bindings,
            origin: (0.0, 0.0),
            chrome_clear: [0.0, 0.0, 0.0, 1.0],
            tex: ImageContent {
                linear_sample: 1,
                ..ImageContent::default()
            },
            prefs: SessionPrefs {
                clear: 0,
                clear_lin: [0.0, 0.0, 0.0, 1.0],
                background: Background::Black,
                background_override: None,
                outline: true,
                octagon: crate::octagon::OctagonState::default(),
                fit_upscale,
                open_actual_size: false,
                default_tonemap: Tonemap::Reinhard,
                // Off until the shell pushes the config in — which it does immediately after
                // construction, via `apply_view_config`.
                zoom_snaps: Vec::new(),
                zoom_snap_px: 0.0,
            },
            generation: 0,
            anim: AnimState::default(),
            viewport: Viewport::new(width.max(1), height.max(1)),
            view: ViewState::default(),
            flipbook: None,
            display: DisplayState::default(),
            gesture: GestureState::default(),
        })
    }

    /// Set the letterbox / no-image backdrop color (packed `0x00RRGGBB`); stored both packed and
    /// as linear floats so the shader's encode brings it back to the intended sRGB.
    pub fn set_clear(&mut self, packed: u32) {
        self.prefs.clear = packed;
        let dec = |b: u32| srgb_to_linear((b & 0xff) as f32 / 255.0);
        self.prefs.clear_lin = [dec(packed >> 16), dec(packed >> 8), dec(packed), 1.0];
    }

    /// Take a fresh decode generation for this window and return it.
    pub fn next_generation(&mut self) -> u64 {
        self.generation = crate::decode_pool::fresh_generation();
        self.generation
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Adopt a generation issued elsewhere (the launch path's decode, submitted before this
    /// window existed).
    pub fn set_generation(&mut self, generation: u64) {
        self.generation = generation;
    }

    pub fn current_image(&self) -> Option<&DecodedImage> {
        self.tex.current.as_deref()
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
        self.tex.current.as_ref().is_some_and(|i| i.format.is_hdr())
    }

    /// Whether the current image carries an alpha channel (gray+A or RGBA source) — drives the
    /// RGB↔RGBA toolbar icon and keeps the alpha-channel isolation control available. This is true
    /// even when the alpha is entirely opaque, so the user can always inspect it; whether that
    /// alpha actually holds transparency (and thus defaults to the checker backdrop) is a separate
    /// signal handled in [`Self::set_image`] via `DecodedImage::alpha_opaque`.
    pub fn has_alpha(&self) -> bool {
        self.tex
            .current
            .as_ref()
            .is_some_and(|i| matches!(i.channels, 2 | 4))
    }

    pub fn background(&self) -> Background {
        self.prefs.background
    }

    /// Set the viewport backdrop (toolbar override) and repaint. Records the pick so it persists
    /// across image navigation for the rest of the session (see `background_override`).
    pub fn set_background(&mut self, bg: Background) {
        self.prefs.background = bg;
        self.prefs.background_override = Some(bg);
        self.invalidate();
    }

    /// Apply the configured backdrop preference (settings dialog / startup): `Some` pins that
    /// backdrop for every image, `None` restores the per-image default (checker for real
    /// transparency, black otherwise) — including for the image already on screen.
    pub fn set_background_pref(&mut self, bg: Option<Background>) {
        self.prefs.background_override = bg;
        self.prefs.background = bg.unwrap_or_else(|| {
            self.tex
                .current
                .as_ref()
                .map_or(Background::Black, |img| default_background(img))
        });
        self.invalidate();
    }

    /// Whether the explicit fit command upscales small images (the `fit-upscale` config key).
    pub fn set_fit_upscale(&mut self, on: bool) {
        self.prefs.fit_upscale = on;
    }

    /// The zoom's detents, shared by the right-drag, the wheel and the zoom keys: the levels to
    /// snap to as *percentages* (`zoom-snap-levels`, converted to zoom factors here) and how far
    /// past one the drag has to travel to break out, in drag px (`zoom-snap`, read as the detent
    /// width by the discrete steps — see [`Self::zoom_release`]). Either empty levels or a zero
    /// distance switches snapping off.
    pub fn set_zoom_snapping(&mut self, levels_pct: &[f32], strength_px: f32) {
        self.prefs.zoom_snaps = levels_pct.iter().map(|p| p / 100.0).collect();
        self.prefs.zoom_snap_px = strength_px;
    }

    /// Whether a newly opened image lands at native 1:1 rather than fitted (`default-fit`). Takes
    /// effect on the next adopt — it never yanks the view of the image already on screen.
    pub fn set_open_actual_size(&mut self, on: bool) {
        self.prefs.open_actual_size = on;
    }

    /// The tonemap a newly adopted image starts on (`default-tonemap`). Like `set_open_actual_size`,
    /// this seeds the *next* image: the current one keeps whatever the user toggled it to.
    pub fn set_default_tonemap(&mut self, t: Tonemap) {
        self.prefs.default_tonemap = t;
    }

    pub fn outline(&self) -> bool {
        self.prefs.outline
    }

    /// Toggle the image-boundary outline and repaint.
    pub fn toggle_outline(&mut self) {
        self.prefs.outline = !self.prefs.outline;
        self.invalidate();
    }

    /// Apply the configured outline preference (settings dialog / startup). The outline is one
    /// session-global flag, not per-image state, so — like [`Self::set_background_pref`] — this
    /// reaches the image already on screen rather than seeding the next one.
    pub fn set_outline(&mut self, on: bool) {
        self.prefs.outline = on;
        self.invalidate();
    }

    pub fn octagon(&self) -> crate::octagon::OctagonState {
        self.prefs.octagon
    }

    /// Adopt the octagon overlay's state (the options window's edits, or the persisted config at
    /// startup), clamped, and repaint.
    pub fn set_octagon(&mut self, mut s: crate::octagon::OctagonState) {
        s.clamp();
        self.prefs.octagon = s;
        self.invalidate();
    }

    /// Toggle the octagon overlay (toolbar) and repaint. The options — color, crop, hide — survive
    /// the off state, so re-enabling picks up where the user left off.
    pub fn toggle_octagon(&mut self) {
        self.prefs.octagon.enabled = !self.prefs.octagon.enabled;
        self.invalidate();
    }

    /// The displayed frame's rect in **image-region** coords (pan/zoom applied): the whole image,
    /// or the current cell in flipbook mode. `None` with no image. This is what the octagon line
    /// overlay is drawn against.
    pub fn frame_screen_rect(&self) -> Option<(f32, f32, f32, f32)> {
        let dims = self.view_dims()?;
        let (x, y) = self.view.image_to_screen((0.0, 0.0), dims, &self.viewport);
        let (w, h) = self.view.image_screen_size(dims);
        Some((x, y, w, h))
    }

    fn image_dims(&self) -> Option<(u32, u32)> {
        self.tex.current.as_ref().map(|i| (i.width, i.height))
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
        self.invalidate();
    }

    /// Update only the fractional playback position (the hot path: playback tick / slider scrub).
    /// No re-fit; just repaint.
    pub fn set_flipbook_pos(&mut self, frame_pos: f32) {
        if let Some(fb) = &mut self.flipbook {
            fb.frame_pos = frame_pos;
            self.invalidate();
        }
    }

    /// Drop the displayed image so the next paint shows the placeholder. Also drops any animation
    /// frames so the viewer's next `frame_delay_ms()` returns `None` and the playback timer stops.
    pub fn clear_image(&mut self) {
        self.tex.current = None;
        self.tex.release();
        self.bindings = make_bindings(&self.gpu, self.gpu.placeholder_view);
        self.anim.clear();
        self.flipbook = None;
    }

    /// Adopt a decoded image: upload it (level 0 plus `mips`, the chain the decode worker built)
    /// as a GPU texture and reset to fit + neutral display state for the new file (#17). Returns
    /// the GPU error if the upload fails (e.g. out of memory on a very large image) so the caller
    /// can report it instead of the process aborting; on failure the prior display state is left
    /// untouched.
    pub fn set_image(&mut self, img: Arc<DecodedImage>, mips: &[Vec<u8>]) -> Result<(), String> {
        let (w, h) = (img.width, img.height);
        // Upload first: if the GPU rejects the texture we bail here, before mutating any state,
        // so a failed adopt can't leave the surface half-updated.
        self.upload_texture(&img, mips)?;
        // Adopt any animation frames for playback and start from frame 0 (already uploaded above).
        // The viewer arms the timer from `frame_delay_ms()` after this.
        self.anim.adopt(&img);
        // Pick the viewport backdrop: an explicit pick (the toolbar's background buttons, or the
        // `background` config key) sticks across every image; otherwise default to the image's
        // nature — see `default_background`.
        self.prefs.background = self
            .prefs
            .background_override
            .unwrap_or_else(|| default_background(&img));
        self.tex.current = Some(img);
        // Neutral display state for the new file (#17), seeded with the configured tonemap and
        // with the composite mode the source's own format asks for: alpha is honored by default
        // whenever there is one, so a transparent PNG opens composited over the backdrop rather
        // than showing whatever colors hide under its zero-alpha pixels.
        self.display = DisplayState {
            tonemap: self.prefs.default_tonemap,
            channel: Channel::composite(self.has_alpha()),
            ..DisplayState::default()
        };
        // A fresh image starts as a whole-image view; the viewer re-applies any per-path
        // flipbook state (via `set_flipbook`) right after this adopt, which re-fits to the frame.
        self.flipbook = None;
        // Every newly opened image (including folder ←/→ navigation) fits *without* upscaling: a
        // large image shrinks to fit, a small one shows at native 1:1. The explicit fit command
        // (`F` / toolbar) can still fill the surface — see `fit_upscale` / `fit`. With
        // `default-fit = "actual-size"` an image instead opens at 100% however large it is.
        if self.prefs.open_actual_size {
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
    pub fn replace_image_keep_view(
        &mut self,
        img: Arc<DecodedImage>,
        mips: &[Vec<u8>],
    ) -> Result<(), String> {
        self.upload_texture(&img, mips)?;
        // Refresh the animation frames from the re-decoded file and restart from frame 0 (the view
        // is preserved, but the animation plays from the top). The viewer re-arms the timer.
        self.anim.adopt(&img);
        self.tex.current = Some(img);
        // A re-export can drop the alpha channel. Both alpha-dependent modes stop meaning anything
        // then — and the toolbar's A button is gone, so an alpha solo would be a state with no way
        // out — so fall back to the composite the new file does have.
        if !self.has_alpha() && matches!(self.display.channel, Channel::Rgba | Channel::A) {
            self.display.channel = Channel::Rgb;
        }
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
    /// `None` for a still image. The viewer arms the playback timer from this after every adopt.
    pub fn frame_delay_ms(&self) -> Option<u32> {
        self.anim.delay_ms()
    }

    /// Advance to the next animation frame (wrapping) and upload it as the texture, returning the
    /// now-current frame's delay (ms) so the caller can reschedule the timer (GIF frame delays
    /// vary). Returns `None` and does nothing for a still image. On a GPU upload error the visible
    /// frame is left unchanged and the current frame's delay is returned, so a transient failure
    /// paces the retry rather than wedging playback. The frame's mip chain is built here, on the
    /// UI thread: GIF frames are small.
    pub fn advance_frame(&mut self) -> Option<u32> {
        let n = self.anim.frames().len();
        if n <= 1 {
            return None;
        }
        let (w, h) = self.image_dims()?;
        let format = self.tex.current.as_ref()?.format;
        let next = (self.anim.index + 1) % n;
        match create_image_texture(
            &self.gpu,
            &self.anim.frames()[next].pixels,
            w,
            h,
            format,
            None,
        ) {
            Ok((image, view, linear)) => {
                self.adopt_texture(image, view, linear);
                self.anim.index = next;
            }
            Err(e) => eprintln!("fire: animation frame upload failed: {e}"),
        }
        self.anim.delay_ms()
    }

    /// Upload `img`'s (frame-0) pixels as a texture with its full mip chain. Returns the GPU
    /// error rather than panicking if creation fails — this runs synchronously on the UI thread.
    fn upload_texture(&mut self, img: &DecodedImage, mips: &[Vec<u8>]) -> Result<(), String> {
        let (image, view, linear_sample) = create_image_texture(
            &self.gpu,
            &img.pixels,
            img.width,
            img.height,
            img.format,
            Some(mips),
        )?;
        self.adopt_texture(image, view, linear_sample);
        Ok(())
    }

    /// Install a freshly created texture and rebuild the bindings around it.
    fn adopt_texture(&mut self, image: sg::Image, view: sg::View, linear_sample: i32) {
        self.tex.release();
        self.bindings = make_bindings(&self.gpu, view);
        self.tex.image = Some(image);
        self.tex.view = Some(view);
        self.tex.linear_sample = linear_sample;
    }

    /// The client changed size (physical px): resize the swapchain to match. The image's sub-rect
    /// within it is a separate concern — the viewer calls [`Self::set_image_rect`] right after,
    /// because only it knows how tall the chrome is.
    pub fn resize(&mut self, width: u32, height: u32) {
        self.swapchain.resize(width, height);
        self.invalidate();
    }

    /// Schedule a repaint (delivered as `RedrawRequested`).
    pub fn invalidate(&self) {
        self.window.request_redraw();
    }

    /// The image's sub-rect of the client, in physical px. The chrome owns the rest.
    pub fn set_image_rect(&mut self, x: f32, y: f32, w: f32, h: f32) {
        // The early-out must compare against what `Viewport::new` will actually store — it
        // clamps each axis to at least 1 px — or a legitimately zero-size region never matches
        // the stored 1.0 and every frame re-enters, re-running fit_to_window against a 1-px
        // viewport and driving the zoom to its floor. Unreachable today (the minimum window size
        // keeps the client big enough), which is exactly why it would go unnoticed when a
        // future layout change makes it reachable.
        let (w, h) = (
            (w.max(0.0) as u32).max(1) as f32,
            (h.max(0.0) as u32).max(1) as f32,
        );
        if self.origin == (x, y) && self.viewport.width == w && self.viewport.height == h {
            return;
        }
        self.origin = (x, y);
        self.viewport = Viewport::new(w as u32, h as u32);
        if let Some(dims) = self.view_dims() {
            if self.view.fit {
                self.view
                    .fit_to_window(dims, &self.viewport, self.view.fit_upscale);
            } else {
                self.view.clamp_pan(dims, &self.viewport);
            }
        }
    }

    /// The image sub-rect's origin in client coords — the shell subtracts it before handing us
    /// cursor positions, so all the pan/zoom math stays in image-region space.
    pub fn image_origin(&self) -> (f32, f32) {
        self.origin
    }

    /// The chrome fill (sRGB): what the parts of the frame the image doesn't cover get cleared to.
    pub fn set_chrome_clear(&mut self, rgba: [f32; 4]) {
        self.chrome_clear = [rgba[0], rgba[1], rgba[2], 1.0];
    }

    /// Draw one frame: clear to the chrome fill, the image into its sub-rect, then `ui` over the
    /// whole client, all in one swapchain pass, and present it.
    ///
    /// The image is **one fullscreen triangle**: the viewport and scissor map NDC onto the sub-rect
    /// and clip to it, so the shader (background, checkerboard, letterbox and all) fills exactly
    /// the image region and nothing else. No per-pixel CPU work, no extra draws.
    pub fn render_frame(&mut self, ui: impl FnOnce()) -> Presented {
        let (sw, sh) = self.swapchain.size();
        if sw == 0 || sh == 0 {
            return Presented::Skipped;
        }
        let Some(render_view) = self.swapchain.render_view() else {
            return Presented::Skipped;
        };
        let mut swapchain = sg::Swapchain::new();
        swapchain.width = sw as i32;
        swapchain.height = sh as i32;
        swapchain.sample_count = 1;
        swapchain.color_format = SWAPCHAIN_FORMAT;
        swapchain.depth_format = sg::PixelFormat::None;
        swapchain.d3d11.render_view = render_view;
        let mut pass = sg::Pass::new();
        pass.swapchain = swapchain;
        let c = self.chrome_clear;
        pass.action.colors[0] = sg::ColorAttachmentAction {
            load_action: sg::LoadAction::Clear,
            store_action: sg::StoreAction::Store,
            clear_value: sg::Color {
                r: c[0],
                g: c[1],
                b: c[2],
                a: 1.0,
            },
        };
        pass.label = c"fire frame".as_ptr();
        sg::begin_pass(&pass);

        // The viewport *is* the image's sub-rect, clamped to the framebuffer (a mid-resize frame
        // can briefly disagree with it).
        let (cw, ch) = (swapchain.width as f32, swapchain.height as f32);
        let x = self.origin.0.clamp(0.0, cw);
        let y = self.origin.1.clamp(0.0, ch);
        let w = self.viewport.width.min(cw - x);
        let h = self.viewport.height.min(ch - y);
        if w >= 1.0 && h >= 1.0 {
            sg::apply_viewportf(x, y, w, h, true);
            sg::apply_scissor_rectf(x, y, w, h, true);
            sg::apply_pipeline(self.gpu.pipeline);
            sg::apply_bindings(&self.bindings);
            let params = self.build_params();
            sg::apply_uniforms(0, &sg::value_as_range(&params));
            sg::draw(0, 3, 1);
            sg::apply_viewportf(0.0, 0.0, cw, ch, true);
            sg::apply_scissor_rectf(0.0, 0.0, cw, ch, true);
        }

        // The pass must close whatever happens inside the UI: a panic that escaped here would
        // leave sokol_gfx mid-pass and assert on the next frame.
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(ui)).is_err() {
            eprintln!("fire: recovered from a panic while drawing the UI");
        }

        sg::end_pass();
        sg::commit();
        let t0 = Instant::now();
        let looking = self.swapchain.present();
        // Did the present block on the display? A vblank is ≥4 ms even at 240 Hz; a present that
        // did not wait returns in microseconds.
        let waited = looking && t0.elapsed() >= Duration::from_micros(500);
        if self.tex.current.is_some() {
            // Measurement hook: inert unless `FIRE_TTFP_OUT` is set (see `crate::ttfp`).
            crate::ttfp::stamp_first_pixel();
        }
        Presented::Yes { waited }
    }

    /// Resolve what the shader should sample: the (possibly fractional) source rect, whether
    /// there is an image at all, and — in flipbook mode — which cell(s) of the sheet to blend.
    ///
    /// Flipbook mode maps the surface into a single frame rect: `img_w/img_h` become the
    /// (fractional) cell size and the fb_* fields pick which cell(s) of the sheet to sample. Off
    /// (still image / whole sheet), the fb fields are identity so the shader path is untouched.
    fn resolve_source_rect(&self) -> (f32, f32, i32, FlipbookCells) {
        let (img_w, img_h, has_image, fbf) = match self.tex.current.as_ref().zip(self.flipbook) {
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
            None => match &self.tex.current {
                Some(img) => (img.width as f32, img.height as f32, 1, None),
                None => (1.0, 1.0, 0, None),
            },
        };
        (img_w, img_h, has_image, fbf)
    }

    /// Build the frame's 128-byte uniform block from the current view, display and session
    /// state. Pure reads — everything the shader needs for one frame, and nothing else.
    fn build_params(&self) -> Params {
        let is_hdr = self.is_hdr();
        let (img_w, img_h, has_image, fbf) = self.resolve_source_rect();
        // Identity flipbook fields when off (fb_on == 0 → shader ignores them, but keep them sane).
        let (sheet_w, sheet_h, ca, cb, fb_blend, fb_max_lod, fb_on) = match fbf {
            Some((sheet, ca, cb, blend, lod)) => {
                (sheet.0 as f32, sheet.1 as f32, ca, cb, blend, lod, 1)
            }
            None => (img_w, img_h, (0.0, 0.0), (0.0, 0.0), 0.0, f32::MAX, 0),
        };
        // Every field is set explicitly (no `..default()`), matching the checker note.
        Params {
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
            linear_sample: self.tex.linear_sample,
            background: background_code(self.prefs.background),
            outline: self.prefs.outline as i32,
            fb_on,
            clear_r: self.prefs.clear_lin[0],
            clear_g: self.prefs.clear_lin[1],
            clear_b: self.prefs.clear_lin[2],
            clear_a: 1.0,
            sheet_w,
            sheet_h,
            cell_a_x: ca.0,
            cell_a_y: ca.1,
            cell_b_x: cb.0,
            cell_b_y: cb.1,
            fb_blend,
            fb_max_lod,
            surf_origin_x: self.origin.0,
            surf_origin_y: self.origin.1,
            oct_crop: self.prefs.octagon.crop,
            oct_hide: if self.prefs.octagon.enabled {
                self.prefs.octagon.hide
            } else {
                0.0
            },
        }
    }

    // --- input-driven view controls (called from the viewer) ----------------

    pub fn on_cursor_moved(&mut self, pos: (f32, f32)) {
        let delta = (pos.0 - self.gesture.cursor.0, pos.1 - self.gesture.cursor.1);
        self.gesture.cursor = pos;
        if self.gesture.dragging {
            if let Some(dims) = self.view_dims() {
                self.view.pan_by(delta, dims, &self.viewport);
                self.invalidate();
            }
        } else if self.gesture.zoom_dragging {
            // Past the click slop this is a real zoom-drag, so the release won't open the menu.
            if !self.gesture.zoom_dragged {
                let (ax, ay) = (
                    pos.0 - self.gesture.zoom_anchor.0,
                    pos.1 - self.gesture.zoom_anchor.1,
                );
                if (ax * ax + ay * ay).sqrt() > ZOOM_DRAG_CLICK_SLOP {
                    self.gesture.zoom_dragged = true;
                }
            }
            // Vertical drag = scrubby zoom about the fixed press anchor (down zooms in, up out),
            // detenting on the round zoom levels and on fit-to-window as it passes them.
            let dy = pos.1 - self.gesture.zoom_last_y;
            self.gesture.zoom_last_y = pos.1;
            if dy != 0.0 {
                if let Some(dims) = self.view_dims() {
                    // The break-out distance is configured in drag px; the detent works in the same
                    // log-zoom units the drag accumulates in, so it converts the same way dy does.
                    let release = self.zoom_release();
                    let zoom = self.gesture.zoom_detent.step(
                        self.view.zoom,
                        dy * ZOOM_DRAG_SENSITIVITY,
                        &self.prefs.zoom_snaps,
                        release,
                    );
                    self.view
                        .zoom_to(zoom, self.gesture.zoom_anchor, dims, &self.viewport);
                    self.invalidate();
                }
            }
        }
    }

    pub fn begin_drag(&mut self) {
        self.gesture.dragging = true;
    }

    pub fn end_drag(&mut self) {
        self.gesture.dragging = false;
    }

    /// A pan or zoom drag is in progress, i.e. the image owns the mouse until the button comes up —
    /// even if the cursor has wandered over the toolbar. Without this the shell would hand the drag
    /// to ImGui mid-gesture the moment the pointer crossed the chrome, and the pan would stick.
    pub fn is_mouse_captured(&self) -> bool {
        self.gesture.is_mouse_captured()
    }

    /// Begin an RMB zoom-drag, pivoting on the current cursor (the press point).
    pub fn begin_zoom_drag(&mut self) {
        self.gesture.begin_zoom_drag();
    }

    /// End an RMB gesture. Returns `true` if it was an actual zoom-drag (the cursor moved past
    /// [`ZOOM_DRAG_CLICK_SLOP`]); `false` if it was effectively a right-click, so the caller can
    /// open the context menu instead.
    pub fn end_zoom_drag(&mut self) -> bool {
        self.gesture.end_zoom_drag()
    }

    /// Whether an RMB zoom-drag is in progress (the shell repaints the zoom % while it is).
    pub fn is_zoom_dragging(&self) -> bool {
        self.gesture.zoom_dragging
    }

    /// The detent break-out distance in the natural-log zoom units both zoom paths work in.
    ///
    /// The config states it in *drag pixels* (`zoom-snap`), which is what makes it tangible for the
    /// gesture it was written for. Converting through the drag's own sensitivity — rather than
    /// giving the wheel a second, unrelated knob — is what makes one setting describe one detent
    /// width, however you reach it.
    fn zoom_release(&self) -> f32 {
        self.prefs.zoom_snap_px * ZOOM_DRAG_SENSITIVITY
    }

    pub fn zoom_at_cursor(&mut self, factor: f32) {
        if let Some(dims) = self.view_dims() {
            let release = self.zoom_release();
            self.view.zoom_to_cursor(
                factor,
                self.gesture.cursor,
                dims,
                &self.viewport,
                &self.prefs.zoom_snaps,
                release,
            );
            self.invalidate();
        }
    }

    pub fn zoom_centered(&mut self, factor: f32) {
        if let Some(dims) = self.view_dims() {
            let release = self.zoom_release();
            self.view.zoom_centered(
                factor,
                dims,
                &self.viewport,
                &self.prefs.zoom_snaps,
                release,
            );
            self.invalidate();
        }
    }

    pub fn fit(&mut self) {
        if let Some(dims) = self.view_dims() {
            self.view
                .fit_to_window(dims, &self.viewport, self.prefs.fit_upscale);
            self.invalidate();
        }
    }

    pub fn one_to_one(&mut self) {
        self.view.one_to_one();
        if let Some(dims) = self.view_dims() {
            self.view.clamp_pan(dims, &self.viewport);
        }
        self.invalidate();
    }

    /// Solo one channel, or switch the solo back off — which lands on the image's composite mode
    /// ([`Channel::composite`]), not unconditionally on `Rgb`: leaving an alpha image's R solo
    /// puts you back where the image opened.
    pub fn toggle_channel(&mut self, ch: Channel) {
        self.display.channel = if self.display.channel == ch {
            Channel::composite(self.has_alpha())
        } else {
            ch
        };
        self.invalidate();
    }

    /// The composite button / `all-channels` key: flip RGBA↔RGB when a composite mode is already
    /// showing, or return to the image's default composite from a single-channel solo. A source
    /// without alpha has only `Rgb`, so for it this stays the plain "all channels" reset.
    pub fn toggle_composite(&mut self) {
        self.display.channel = if self.display.channel == Channel::Rgba {
            Channel::Rgb
        } else {
            // From RGB this flips to RGBA (or stays RGB with no alpha to composite); from a solo
            // it returns to the mode the image opened in.
            Channel::composite(self.has_alpha())
        };
        self.invalidate();
    }

    pub fn adjust_exposure(&mut self, delta: f32) {
        self.display.exposure = (self.display.exposure + delta).clamp(-16.0, 16.0);
        self.invalidate();
    }

    pub fn reset_exposure(&mut self) {
        self.display.exposure = 0.0;
        self.invalidate();
    }

    pub fn toggle_tonemap(&mut self) {
        self.display.tonemap = match self.display.tonemap {
            Tonemap::Reinhard => Tonemap::Aces,
            Tonemap::Aces => Tonemap::Reinhard,
        };
        self.invalidate();
    }
}

impl Drop for GpuSurface {
    fn drop(&mut self) {
        if sg::isvalid() {
            self.tex.release();
        }
    }
}

/// The frame's bindings: the image texture (or the placeholder) at view slot 0, the two samplers.
fn make_bindings(gpu: &Gpu, view: sg::View) -> sg::Bindings {
    let mut b = sg::Bindings::new();
    b.views[0] = view;
    b.samplers[0] = gpu.samp_aniso;
    b.samplers[1] = gpu.samp_point;
    b
}

fn channel_code(ch: Channel) -> i32 {
    match ch {
        Channel::Rgba => 0,
        Channel::R => 1,
        Channel::G => 2,
        Channel::B => 3,
        Channel::A => 4,
        Channel::Rgb => 5,
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

/// sRGB→linear for a single component (matches the shader), used for the clear colors.
fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Build an immutable image (+ its texture view) from one RGBA frame and its mip chain — `mips`
/// as [`crate::render::mips::build`] makes it, or `None` to build it here — returning the image,
/// its view, and the `linear_sample` flag for `format` (1 if the sample is already linear — 8-bit
/// sRGB / float — 0 if the shader must sRGB-decode 16-bit unorm). A free function (not a method)
/// so the per-frame animation upload can borrow pixels straight out of [`AnimState`] without
/// aliasing the `&mut self` receiver. Returns the GPU error instead of panicking (this runs
/// synchronously on the UI thread).
fn create_image_texture(
    gpu: &Gpu,
    pixels: &[u8],
    width: u32,
    height: u32,
    format: PixelFormat,
    mips: Option<&[Vec<u8>]>,
) -> Result<(sg::Image, sg::View, i32), String> {
    // The dimensions and the buffer arrive from different producers (decode headers vs the
    // pixel Vec — part of it native FFI); a short buffer is refused here rather than read past.
    let src_bpp = mips::bytes_per_texel(format);
    let needed = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(src_bpp));
    if needed.is_none_or(|n| pixels.len() < n) {
        return Err("pixel buffer is shorter than its dimensions declare".into());
    }
    let level0 = &pixels[..needed.unwrap_or(0)];
    let levels = mips::level_count(width, height) as usize;
    let built;
    let chain: &[Vec<u8>] = match mips {
        Some(m) if m.len() + 1 == levels => m,
        // No chain (an animation frame), or one that does not match this image: build it now.
        _ => {
            built = mips::build(level0, width, height, format);
            &built
        }
    };
    if chain.len() + 1 != levels {
        return Err("the mip chain is incomplete".into());
    }

    // The device format, the shader's decode flag, and a conversion for the pixels when the
    // device lacks the feature the source format needs.
    let (tex_format, linear_sample, convert): (sg::PixelFormat, i32, Option<Convert>) = match format
    {
        // 8-bit sources are sRGB-encoded; the sRGB format decodes to linear on sample.
        PixelFormat::Rgba8Unorm => (sg::PixelFormat::Srgb8a8, 1, None),
        // 16-bit unorm is treated as sRGB-encoded (matches the CPU path) → decode in shader.
        // Without a 16-bit-norm format it rides as float16 of the same 0..1 values.
        PixelFormat::Rgba16Unorm if gpu.norm16 => (sg::PixelFormat::Rgba16, 0, None),
        PixelFormat::Rgba16Unorm => (sg::PixelFormat::Rgba16f, 0, Some(mips::u16_to_f16)),
        // Float sources are already linear.
        PixelFormat::Rgba16Float => (sg::PixelFormat::Rgba16f, 1, None),
        PixelFormat::Rgba32Float if gpu.float32_filterable => (sg::PixelFormat::Rgba32f, 1, None),
        // No float32 filtering on this device: float16 keeps the pipeline (and the mips)
        // filtered, at the cost of range above 65504.
        PixelFormat::Rgba32Float => (sg::PixelFormat::Rgba16f, 1, Some(mips::f32_to_f16)),
    };
    let converted: Vec<Vec<u8>> = match convert {
        Some(f) => std::iter::once(level0)
            .chain(chain.iter().map(Vec::as_slice))
            .map(f)
            .collect(),
        None => Vec::new(),
    };

    let mut desc = sg::ImageDesc::new();
    desc._type = sg::ImageType::Dim2;
    desc.width = width as i32;
    desc.height = height as i32;
    desc.num_mipmaps = levels as i32;
    desc.pixel_format = tex_format;
    desc.label = c"fire image".as_ptr();
    if convert.is_some() {
        for (i, lvl) in converted.iter().enumerate() {
            desc.data.mip_levels[i] = sg::slice_as_range(lvl);
        }
    } else {
        desc.data.mip_levels[0] = sg::slice_as_range(level0);
        for (i, lvl) in chain.iter().enumerate() {
            desc.data.mip_levels[i + 1] = sg::slice_as_range(lvl);
        }
    }
    let image = sg::make_image(&desc);
    if sg::query_image_state(image) != sg::ResourceState::Valid {
        sg::destroy_image(image);
        return Err(format!(
            "the GPU refused a {width}×{height} {tex_format:?} texture with {levels} mip levels"
        ));
    }
    let mut vd = sg::ViewDesc::new();
    vd.texture.image = image;
    vd.label = c"fire image".as_ptr();
    let view = sg::make_view(&vd);
    if sg::query_view_state(view) != sg::ResourceState::Valid {
        sg::destroy_view(view);
        sg::destroy_image(image);
        return Err("the image's texture view could not be created".into());
    }
    Ok((image, view, linear_sample))
}
