//! The Metal device and the `CAMetalLayer` sokol_gfx draws through on macOS.
//!
//! sokol_gfx does not own a window: it is handed a device at `sg_setup` (through
//! `sg_environment`) and a render target per frame (through `sg_swapchain`), and the shell owns
//! everything around them. This module is that glue — the twin of [`crate::render::d3d11`], same
//! shape, same contract — and the one place on macOS that names Metal or Core Animation.
//!
//! Three things differ from the D3D11 side, and each is load-bearing:
//!
//! * **The backbuffer is `BGRA8Unorm`, not `RGBA8Unorm`.** A `CAMetalLayer` only accepts a short
//!   list of formats and RGBA8 is not on it. This is a channel *order* difference in storage
//!   only: the shader still writes float4 RGBA and Metal swizzles on the way out, so D20 holds
//!   unchanged — a plain UNORM target that the image shader sRGB-encodes itself.
//! * **sokol presents the drawable, not us.** `sg_end_pass` calls `presentDrawable:` on whatever
//!   `sg_swapchain` was pointed at and `sg_commit` commits the buffer, so [`Swapchain::present`]
//!   only releases our hold on the drawable. Presenting again here would be a double present.
//! * **The frame blocks at acquire, not at present.** `nextDrawable` waits until the display has
//!   taken an earlier frame; D3D11 waits inside `Present(1, 0)` instead. Both are "the handoff
//!   blocked on the display", which is what playback is paced on, so [`Swapchain::present`]
//!   reports the wait this module measured in [`Swapchain::acquire`].
//!
//! The device is created on the bring-up thread (see [`crate::render::gpu::Gpu::start`]) and used
//! from the main thread after the join. The *layer* is not: Core Animation and `NSView` are
//! main-thread-only, so [`Swapchain::new`] must be called there, as its caller (`GpuSurface::new`)
//! is.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_app_kit::NSView;
use objc2_foundation::CGSize;
use objc2_metal::{
    MTLCreateSystemDefaultDevice, MTLDevice, MTLGPUFamily, MTLOrigin, MTLPixelFormat, MTLRegion,
    MTLResourceOptions, MTLSize, MTLTexture, MTLTextureDescriptor, MTLTextureUsage,
};
use objc2_quartz_core::{CAMetalDrawable, CAMetalLayer};
use sokol::gfx as sg;
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window;

use crate::render::mips::Convert;

/// What every window's backbuffer is created as, and what sokol_gfx is told to expect of a
/// swapchain pass, so the viewport and ImGui pipelines match it. `BGRA8`, not the D3D11 side's
/// `RGBA8`: see the module header.
pub const SWAPCHAIN_FORMAT: sg::PixelFormat = sg::PixelFormat::Bgra8;

/// How many drawables may be in flight. Two, to mirror the D3D11 side's two-buffer flip chain:
/// with three, `nextDrawable` only blocks once three frames are queued, which loosens the pacing
/// signal playback depends on (see the module header).
const MAX_DRAWABLES: usize = 2;

/// The process's Metal device — what sokol_gfx runs on.
pub struct Device {
    device: Retained<ProtocolObject<dyn MTLDevice>>,
}

// SAFETY: an `MTLDevice` is thread-safe by specification — Metal explicitly allows creating and
// using a device from any thread. The bring-up thread creates it and hands it to the main thread
// through a `JoinHandle`, whose join is the synchronization point.
unsafe impl Send for Device {}

impl Device {
    /// The system's default GPU. Errors come back as strings for the caller to show: this runs at
    /// startup in a process that may have no console, where a panic is an invisible abort.
    ///
    /// There is no WARP-style software fallback to try, as there is on D3D11: every Mac that runs
    /// a supported macOS has a Metal-capable GPU, and a nil device here means the process cannot
    /// render at all.
    pub fn create() -> Result<Device, String> {
        // SAFETY: the standard Metal entry point, valid to call from any thread. It returns a
        // +1 reference (a `Create` rule function), so `Retained::from_raw` takes that ownership
        // rather than retaining again.
        let device = unsafe { Retained::from_raw(MTLCreateSystemDefaultDevice()) };
        device
            .map(|device| Device { device })
            .ok_or_else(|| "no Metal device: this GPU cannot run Fire".to_string())
    }

    /// The raw `MTLDevice` pointer, for `sg_environment`. Not retained by the caller beyond the
    /// device's own lifetime: sokol_gfx retains what it keeps.
    fn raw_device(&self) -> *const c_void {
        Retained::as_ptr(&self.device).cast()
    }

    /// Point `env` at this device, so `sg_setup` runs on it.
    pub fn fill_environment(&self, env: &mut sg::Environment) {
        env.metal = sg::MetalEnvironment {
            device: self.raw_device(),
        };
    }

    /// A handle for building image textures on other threads.
    pub fn uploader(&self) -> Uploader {
        // The storage mode sokol_gfx gives an immutable image of its own (`_sg_mtl_init_texdesc`),
        // so a texture built here samples exactly as one built there: shared on Apple GPUs,
        // managed on Intel ones, which sokol keeps off shared textures.
        let storage = if self.device.supportsFamily(MTLGPUFamily::Apple1) {
            MTLResourceOptions::MTLResourceStorageModeShared
        } else {
            MTLResourceOptions::MTLResourceStorageModeManaged
        };
        Uploader {
            device: self.device.clone(),
            storage,
        }
    }
}

/// Rows of a large level are written by this many threads at once. The copy into a texture is
/// bound by memory bandwidth, not by any one core: a 16384×8192 float level takes ~196 ms on one
/// thread and ~95 ms on four, and more than four only contend (eight ~104 ms, twelve ~170 ms, on
/// an M-series Mac).
const WRITE_THREADS: usize = 4;

/// Below this many bytes a level is written on one thread; the split is not worth its spawns.
const WRITE_SPLIT_MIN: usize = 8 << 20;

/// Builds image textures off the main thread — on the decode worker that decoded the image, so a
/// large upload (a 16k float panorama is 2.7 GB with its mips) never stalls the window.
///
/// Nothing here goes near sokol_gfx, which is single-threaded: this is the Metal device alone,
/// and the finished texture is handed to sokol_gfx on the main thread ([`make_image`]).
#[derive(Clone)]
pub struct Uploader {
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    storage: MTLResourceOptions,
}

// SAFETY: an `MTLDevice` is thread-safe by specification; see `Device`.
unsafe impl Send for Uploader {}
unsafe impl Sync for Uploader {}

/// An image texture built by an [`Uploader`], owned until [`make_image`] hands it to sokol_gfx —
/// or until it is dropped, which frees it (a decode that was superseded on its way to the window).
pub struct NativeTexture(Retained<ProtocolObject<dyn MTLTexture>>);

// SAFETY: a texture is a thread-safe Metal object; the only access before it reaches sokol_gfx is
// `replaceRegion`, from the threads of one `Uploader::upload`, into disjoint rows.
unsafe impl Send for NativeTexture {}
unsafe impl Sync for NativeTexture {}

/// sokol_gfx's pixel formats that image textures use, as Metal's — the same mapping as sokol's
/// own `_sg_mtl_pixel_format`.
fn pixel_format(format: sg::PixelFormat) -> Option<MTLPixelFormat> {
    Some(match format {
        sg::PixelFormat::Srgb8a8 => MTLPixelFormat::RGBA8Unorm_sRGB,
        sg::PixelFormat::Rgba16 => MTLPixelFormat::RGBA16Unorm,
        sg::PixelFormat::Rgba16f => MTLPixelFormat::RGBA16Float,
        sg::PixelFormat::Rgba32f => MTLPixelFormat::RGBA32Float,
        _ => return None,
    })
}

impl Uploader {
    /// Build a `width`×`height` texture of `format` with a full mip chain: level 0 from `level0`
    /// and the rest from `chain`, run here. `bpp` is bytes per texel *as uploaded*; `convert`, if
    /// any, turns source bytes into that format (applied per row band, so it runs in parallel too).
    ///
    /// Level 0 — three quarters of the bytes — is written by [`WRITE_THREADS`] threads *while*
    /// `chain` builds the mips on this one, so the chain costs the upload nothing but its own
    /// levels' copy. Whatever `chain` returned comes back alongside, so a caller whose texture
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
        let levels = crate::render::mips::level_count(width, height);
        let tex = match self.create(width, height, levels, format) {
            Ok(t) => t,
            Err(e) => {
                let (built, r) = chain();
                return (Err(e), built, r);
            }
        };
        let src_bpp = level0.len() / (width as usize * height as usize).max(1);
        let (built, r) = std::thread::scope(|s| {
            s.spawn(|| write_level(&tex, 0, width, height, level0, src_bpp, bpp, convert));
            chain()
        });
        if built.len() + 1 != levels as usize {
            return (Err("the mip chain is incomplete".into()), built, r);
        }
        for (i, data) in built.iter().enumerate() {
            let level = i as u32 + 1;
            let (w, h) = ((width >> level).max(1), (height >> level).max(1));
            write_level(&tex, level, w, h, data, src_bpp, bpp, convert);
        }
        (Ok(tex), built, r)
    }

    fn create(
        &self,
        width: u32,
        height: u32,
        levels: u32,
        format: sg::PixelFormat,
    ) -> Result<NativeTexture, String> {
        let fmt = pixel_format(format).ok_or_else(|| format!("no Metal format for {format:?}"))?;
        // SAFETY: a fresh descriptor, configured through documented properties; the device is
        // thread-safe (see `Uploader`).
        let tex = unsafe {
            let desc =
                MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                    fmt,
                    width as usize,
                    height as usize,
                    false,
                );
            desc.setMipmapLevelCount(levels as usize);
            desc.setUsage(MTLTextureUsage::ShaderRead);
            desc.setResourceOptions(self.storage);
            self.device.newTextureWithDescriptor(&desc)
        };
        tex.map(NativeTexture).ok_or_else(|| {
            format!(
                "the GPU refused a {width}×{height} {format:?} texture with {levels} mip levels"
            )
        })
    }
}

/// Write one whole mip level, in row bands across threads when it is big enough to be worth it.
#[allow(clippy::too_many_arguments)]
fn write_level(
    tex: &NativeTexture,
    level: u32,
    width: u32,
    height: u32,
    src: &[u8],
    src_bpp: usize,
    bpp: usize,
    convert: Option<Convert>,
) {
    let src_row = width as usize * src_bpp;
    let threads = if src.len() >= WRITE_SPLIT_MIN {
        WRITE_THREADS
    } else {
        1
    };
    let band = (height as usize).div_ceil(threads);
    let rows = |y0: usize| {
        let y1 = (y0 + band).min(height as usize);
        let bytes = &src[y0 * src_row..y1 * src_row];
        let converted = convert.map(|f| f(bytes));
        let bytes = converted.as_deref().unwrap_or(bytes);
        let region = MTLRegion {
            origin: MTLOrigin { x: 0, y: y0, z: 0 },
            size: MTLSize {
                width: width as usize,
                height: y1 - y0,
                depth: 1,
            },
        };
        // SAFETY: `bytes` holds exactly `region`'s rows at `width * bpp` bytes each, and lives
        // across the call; `replaceRegion` copies them before returning. Concurrent calls write
        // disjoint rows of a texture no GPU work can reference yet (it is not in sokol_gfx).
        unsafe {
            tex.0.replaceRegion_mipmapLevel_withBytes_bytesPerRow(
                region,
                level as usize,
                NonNull::new_unchecked(bytes.as_ptr().cast_mut().cast()),
                width as usize * bpp,
            );
        }
    };
    if threads == 1 {
        rows(0);
    } else {
        std::thread::scope(|s| {
            for y0 in (0..height as usize).step_by(band).skip(1) {
                s.spawn(move || rows(y0));
            }
            rows(0);
        });
    }
}

/// Hand `tex` to sokol_gfx as the image `desc` describes (its size, format and mip count must be
/// the texture's). sokol_gfx takes the texture over and frees it with the image.
pub fn make_image(tex: NativeTexture, desc: &mut sg::ImageDesc) -> sg::Image {
    // sokol_gfx is built without ARC, and for an injected texture it keeps the reference it is
    // given: it adds the pointer to its resource pool (a retain) and then releases its local one
    // (`_sg_mtl_create_image`). So the +1 we own is handed over rather than released here.
    let raw = Retained::into_raw(tex.0);
    desc.mtl_textures[0] = raw.cast::<c_void>().cast_const();
    let image = sg::make_image(desc);
    if sg::query_image_state(image) != sg::ResourceState::Valid {
        // Refused before sokol_gfx reached the texture (validation, or a format it could not
        // map), so the reference is still ours to release.
        // SAFETY: `raw` came from `Retained::into_raw` above and nothing else has released it.
        drop(unsafe { Retained::from_raw(raw) });
    }
    image
}

/// One window's `CAMetalLayer` and the drawable it is currently rendering into.
pub struct Swapchain {
    layer: Retained<CAMetalLayer>,
    /// This frame's drawable, held from [`Self::acquire`] to [`Self::present`]. sokol_gfx keeps
    /// only a borrowed pointer to it across the pass, so it must stay alive until the command
    /// buffer that presents it has been committed.
    drawable: Option<Retained<ProtocolObject<dyn CAMetalDrawable>>>,
    /// How long [`Self::acquire`] blocked in `nextDrawable` — the pacing signal, reported by
    /// [`Self::present`].
    acquire_wait: Duration,
    width: u32,
    height: u32,
}

impl Swapchain {
    /// Attach a `width`×`height` Metal layer to `window`'s view. Vsync-paced, two drawables in
    /// flight, `BGRA8Unorm`: the closest equivalent of the flip-model chain the D3D11 side makes.
    ///
    /// Must be called on the main thread — it touches `NSView` and Core Animation.
    pub fn new(device: &Device, window: &Window, width: u32, height: u32) -> Result<Self, String> {
        let ns_view = match window.window_handle().map(|h| h.as_raw()) {
            Ok(RawWindowHandle::AppKit(h)) => h.ns_view,
            _ => return Err("the window has no AppKit handle".into()),
        };
        let (width, height) = (width.max(1), height.max(1));

        // SAFETY: `CAMetalLayer::new` is a plain `+[CAMetalLayer layer]`-style allocation, and
        // every setter below is a documented property on the layer it was just handed.
        let layer = unsafe {
            let layer = CAMetalLayer::new();
            layer.setDevice(Some(&device.device));
            layer.setPixelFormat(MTLPixelFormat::BGRA8Unorm);
            // We only ever render into the drawable and never read it back, which lets Core
            // Animation skip making it readable.
            layer.setFramebufferOnly(true);
            layer.setMaximumDrawableCount(MAX_DRAWABLES);
            // Vsync: what makes `nextDrawable` block, and so what paces playback.
            layer.setDisplaySyncEnabled(true);
            layer.setContentsScale(window.scale_factor());
            layer.setDrawableSize(CGSize::new(f64::from(width), f64::from(height)));
            layer
        };

        // SAFETY: winit hands out a live `NSView` for the window it owns, and this runs on the
        // main thread (see the doc comment). Setting the layer *before* `setWantsLayer:` makes
        // the view layer-*hosting* — it draws only what we render — rather than layer-backed,
        // where AppKit would own and redraw the layer's contents itself.
        unsafe {
            let view: &NSView = ns_view.cast().as_ref();
            view.setLayer(Some(&layer));
            view.setWantsLayer(true);
        }

        Ok(Swapchain {
            layer,
            drawable: None,
            acquire_wait: Duration::ZERO,
            width,
            height,
        })
    }

    /// The backbuffer size (physical px).
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Resize the layer's drawables. A zero dimension (a minimized window) is remembered but not
    /// applied — Core Animation refuses it — and the frame is skipped instead.
    pub fn resize(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
        if width == 0 || height == 0 {
            return;
        }
        // SAFETY: a documented property on a live layer.
        unsafe {
            self.layer
                .setDrawableSize(CGSize::new(f64::from(width), f64::from(height)));
        }
    }

    /// Follow the window onto a display with a different backing scale.
    ///
    /// `contentsScale` is how Core Animation maps the layer's *point* bounds — which AppKit sets
    /// from the view — onto the drawable's pixels. `resize` keeps the drawable itself right, but
    /// on its own that is not enough: left at the old display's scale, CA would believe the layer
    /// needs twice (or half) the pixels the drawable actually has and rescale it to fit, so a
    /// window dragged from a Retina display to a 1× one would go soft rather than sharp. The two
    /// have to move together, and winit reports them as two events.
    ///
    /// The D3D11 twin has no counterpart: DXGI has no notion of a scale between the swapchain and
    /// the window, so its buffer size is the whole story.
    pub fn set_scale_factor(&mut self, scale: f64) {
        self.layer.setContentsScale(scale);
    }

    /// Acquire this frame's drawable and point `sc` at it, returning whether there is a frame to
    /// draw. `false` (the drawable timed out, or the window is off-screen) means skip the frame
    /// rather than draw into nothing.
    ///
    /// This is where a Metal frame waits on the display, so the wait is timed here and reported
    /// by [`Self::present`].
    pub fn acquire(&mut self, sc: &mut sg::Swapchain) -> bool {
        let t0 = Instant::now();
        // SAFETY: a documented property on a live layer. `nextDrawable` returns nil rather than
        // blocking forever when the layer is off-screen or the wait times out.
        let drawable = unsafe { self.layer.nextDrawable() };
        self.acquire_wait = t0.elapsed();

        let Some(drawable) = drawable else {
            self.drawable = None;
            return false;
        };
        sc.metal.current_drawable = Retained::as_ptr(&drawable).cast();
        // Held until `present`: sokol_gfx only borrows the pointer across the pass.
        self.drawable = Some(drawable);
        true
    }

    /// Finish the frame and report whether its handoff blocked on the display — the signal
    /// playback is paced on.
    ///
    /// There is no present call here: sokol_gfx's `sg_end_pass` already scheduled
    /// `presentDrawable:` on the frame's command buffer and `sg_commit` committed it. All that is
    /// left is to release our own hold on the drawable, which the command buffer retains for as
    /// long as it needs. The wait being reported happened back in [`Self::acquire`].
    pub fn present(&mut self) -> bool {
        self.drawable = None;
        // A vblank is >= 4 ms even at 240 Hz; an acquire that found a free drawable returns in
        // microseconds. The same threshold the D3D11 side uses on its present.
        self.acquire_wait >= Duration::from_micros(500)
    }
}
