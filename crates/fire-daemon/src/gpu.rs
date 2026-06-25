//! wgpu warm-up and per-window render state.
//!
//! [`GpuContext`] holds the expensive, window-independent objects (instance, adapter,
//! device, queue) created once at daemon startup — this is the cost the resident model
//! pays up front so per-open latency stays near zero (§5, §12). [`WindowState`] holds the
//! surface, pipeline, the per-frame view uniform, and the current image's texture, plus
//! the pan/zoom/fit + channel/exposure/tonemap state driven by input (Phase 3).

use std::borrow::Cow;
use std::sync::Arc;

use bytemuck::Zeroable;
use fire_decode::{DecodedImage, PixelFormat};
use wgpu::util::DeviceExt;
use winit::window::Window;

use crate::render::uniforms::ViewUniform;
use crate::render::view::{Channel, DisplayState, Tonemap, ViewState, Viewport};

/// Window-independent GPU objects, warmed at startup.
pub struct GpuContext {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
}

impl GpuContext {
    pub fn new() -> Self {
        let instance = wgpu::Instance::default();
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            ..Default::default()
        }))
        .expect("no suitable GPU adapter found");

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("fire-device"),
            required_features: wgpu::Features::empty(),
            // Request what the adapter supports so max_texture_dimension matches the
            // device the decoder downscales against.
            required_limits: adapter.limits(),
            ..Default::default()
        }))
        .expect("failed to create wgpu device");

        Self { instance, adapter, device, queue }
    }

    /// Max 2D texture dimension of the live device — the §6 downscale limit. Read from
    /// the device, never hardcoded (it varies by adapter/backend: e.g. DX12 16384 vs
    /// Vulkan 32768 on the same GPU).
    pub fn max_texture_dim(&self) -> u32 {
        self.device.limits().max_texture_dimension_2d
    }
}

/// Surface + pipeline for the pooled window, plus the current image and view state.
pub struct WindowState {
    pub window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    /// Whether the surface format does the final linear→sRGB encode in hardware on
    /// present. When false the shader encodes (FLAG_SRGB_ENCODE).
    surface_is_srgb: bool,
    pipeline: wgpu::RenderPipeline,
    texture_bgl: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    /// Persistent view-uniform buffer + its bind group (group 0), updated each frame via
    /// `write_buffer` — no rebuild needed when the view changes.
    uniform_buf: wgpu::Buffer,
    uniform_bind_group: wgpu::BindGroup,
    /// `None` until the first image is uploaded; the render pass clears-only until then.
    texture_bind_group: Option<wgpu::BindGroup>,
    /// Monotonic per-window decode generation. Bumped on each open; a `DecodeDone` whose
    /// generation is older than this is stale and dropped (so a slow decode can't clobber
    /// a newer one).
    generation: u64,
    /// CPU copy of the currently displayed image, retained for the Phase-4 pixel inspector
    /// (#16). Device-loss recovery still re-decodes from the path (#11).
    current_image: Option<DecodedImage>,

    // --- view / input state (Phase 3) ---
    viewport: Viewport,
    view: ViewState,
    display: DisplayState,
    /// Last known cursor position (surface px) — the anchor for wheel zoom-to-cursor.
    cursor: (f32, f32),
    /// Left button held → drag-pan in progress.
    dragging: bool,
}

impl WindowState {
    pub fn new(ctx: &GpuContext, window: Arc<Window>) -> Self {
        let surface = ctx
            .instance
            .create_surface(window.clone())
            .expect("failed to create surface");

        let size = window.inner_size();
        let mut config = surface
            .get_default_config(&ctx.adapter, size.width.max(1), size.height.max(1))
            .expect("surface not supported by adapter");
        // Prefer an sRGB surface so the hardware does the final linear→sRGB encode on
        // present; the shader works in linear throughout (§ Phase 3 color pipeline).
        let caps = surface.get_capabilities(&ctx.adapter);
        if let Some(srgb) = caps.formats.iter().copied().find(|f| f.is_srgb()) {
            config.format = srgb;
        }
        let surface_is_srgb = config.format.is_srgb();
        config.usage = wgpu::TextureUsages::RENDER_ATTACHMENT;
        surface.configure(&ctx.device, &config);

        let shader = ctx.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("image-shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../shaders/image.wgsl").into()),
        });

        // group 0: the view uniform (vertex reads the transform, fragment the rest).
        let uniform_bgl = ctx.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("view-uniform-bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });

        // group 1: the image texture + sampler (rebuilt per uploaded image).
        let texture_bgl = ctx.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("image-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let pipeline_layout = ctx.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("image-pl"),
            // wgpu 29: bind_group_layouts is &[Option<&_>]; immediate_size (renamed from
            // push constants) is 0 = none.
            bind_group_layouts: &[Some(&uniform_bgl), Some(&texture_bgl)],
            immediate_size: 0,
        });

        let pipeline = ctx.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("image-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: config.format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            // The view quad is a 4-vertex triangle strip (draw 0..4).
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        // Nearest magnification keeps texels crisp when zoomed past 1:1 (this is a texture
        // viewer); linear minification keeps fit/zoom-out smooth.
        let sampler = ctx.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("image-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Linear,
            // wgpu 29: mipmap_filter is its own MipmapFilterMode, distinct from FilterMode.
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });

        let uniform_buf = ctx.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("view-uniform"),
            contents: bytemuck::bytes_of(&ViewUniform::zeroed()),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let uniform_bind_group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("view-uniform-bg"),
            layout: &uniform_bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buf.as_entire_binding(),
            }],
        });

        Self {
            window,
            surface,
            config,
            surface_is_srgb,
            pipeline,
            texture_bgl,
            sampler,
            uniform_buf,
            uniform_bind_group,
            texture_bind_group: None,
            generation: 0,
            current_image: None,
            viewport: Viewport::new(size.width, size.height),
            view: ViewState::default(),
            display: DisplayState::default(),
            cursor: (0.0, 0.0),
            dragging: false,
        }
    }

    /// Bump and return this window's decode generation. The caller tags the decode job
    /// with the returned value; only a `DecodeDone` matching the latest generation is
    /// uploaded.
    pub fn next_generation(&mut self) -> u64 {
        self.generation += 1;
        self.generation
    }

    /// The window's current (latest-issued) decode generation.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// CPU pixels of the displayed image, retained for the pixel inspector (#16).
    /// `None` before the first successful decode. (Read in Phase 4.)
    #[allow(dead_code)]
    pub fn current_image(&self) -> Option<&DecodedImage> {
        self.current_image.as_ref()
    }

    fn image_dims(&self) -> Option<(u32, u32)> {
        self.current_image.as_ref().map(|i| (i.width, i.height))
    }

    /// Drop the displayed image so the render pass clears-only — the placeholder shown
    /// while a freshly-opened file decodes (avoids flashing the previous file's pixels).
    pub fn clear_image(&mut self) {
        self.texture_bind_group = None;
        self.current_image = None;
    }

    /// Upload a decoded image to a GPU texture in its native format, bind it (group 1),
    /// retain the CPU buffer for the inspector (#16), and reset the view to fit + neutral
    /// display state for the new file (#17).
    ///
    /// Texture format by source: 8-bit → `Rgba8UnormSrgb` (hardware sRGB decode on sample);
    /// 16-bit unorm → `Rgba16Unorm` (shader linearizes); float HDR → `Rgba16Float`
    /// (f32 is narrowed to f16 so it is sampler-filterable on every adapter, including the
    /// Phase-5 fallback — the full-precision pixels live on in `current_image`).
    pub fn set_image(&mut self, ctx: &GpuContext, img: DecodedImage) {
        let (w, h) = (img.width, img.height);
        let (data, tex_format, bytes_per_row): (Cow<[u8]>, wgpu::TextureFormat, u32) =
            match img.format {
                PixelFormat::Rgba8Unorm => {
                    (Cow::Borrowed(&img.pixels), wgpu::TextureFormat::Rgba8UnormSrgb, 4 * w)
                }
                PixelFormat::Rgba16Unorm => {
                    (Cow::Borrowed(&img.pixels), wgpu::TextureFormat::Rgba16Unorm, 8 * w)
                }
                PixelFormat::Rgba16Float => {
                    (Cow::Borrowed(&img.pixels), wgpu::TextureFormat::Rgba16Float, 8 * w)
                }
                PixelFormat::Rgba32Float => (
                    Cow::Owned(f32_rgba_to_f16_bytes(&img.pixels)),
                    wgpu::TextureFormat::Rgba16Float,
                    8 * w,
                ),
            };

        let size = wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 };
        let texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("image-texture"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: tex_format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        ctx.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bytes_per_row),
                rows_per_image: Some(h),
            },
            size,
        );
        // `data` may borrow img.pixels; drop it before moving img into current_image.
        drop(data);

        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("image-bind-group"),
            layout: &self.texture_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        self.texture_bind_group = Some(bind_group);
        self.current_image = Some(img);

        // New file → reset to fit + neutral channels/exposure/tonemap (#17). The window is
        // resized to the image right after this (decode_done), and that Resized re-fits
        // against the final surface size, so a stale viewport here self-corrects.
        self.display = DisplayState::default();
        self.view.fit_to_window((w, h), &self.viewport);
        self.update_uniform(ctx);
    }

    pub fn resize(&mut self, ctx: &GpuContext, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&ctx.device, &self.config);
        self.viewport = Viewport::new(width, height);
        if let Some(dims) = self.image_dims() {
            if self.view.fit {
                self.view.fit_to_window(dims, &self.viewport);
            } else {
                self.view.clamp_pan(dims, &self.viewport);
            }
        }
        self.update_uniform(ctx);
    }

    /// Rebuild the view uniform from the current view/display/image/viewport and upload it.
    fn update_uniform(&mut self, ctx: &GpuContext) {
        let (dims, format) = self
            .current_image
            .as_ref()
            .map(|i| ((i.width, i.height), i.format))
            .unwrap_or(((1, 1), PixelFormat::Rgba8Unorm));
        let u = ViewUniform::build(
            &self.view,
            &self.display,
            dims,
            &self.viewport,
            format,
            self.surface_is_srgb,
        );
        ctx.queue.write_buffer(&self.uniform_buf, 0, bytemuck::bytes_of(&u));
    }

    fn refresh(&mut self, ctx: &GpuContext) {
        self.update_uniform(ctx);
        self.window.request_redraw();
    }

    // --- input-driven view controls (called from app.rs) ----------------------

    /// Track the cursor; if a drag is in progress, pan by the movement delta.
    pub fn on_cursor_moved(&mut self, ctx: &GpuContext, pos: (f32, f32)) {
        let delta = (pos.0 - self.cursor.0, pos.1 - self.cursor.1);
        self.cursor = pos;
        if self.dragging {
            if let Some(dims) = self.image_dims() {
                self.view.pan_by(delta, dims, &self.viewport);
                self.refresh(ctx);
            }
        }
    }

    pub fn begin_drag(&mut self) {
        self.dragging = true;
    }

    pub fn end_drag(&mut self) {
        self.dragging = false;
    }

    /// Wheel zoom anchored at the last cursor position.
    pub fn zoom_at_cursor(&mut self, ctx: &GpuContext, factor: f32) {
        if let Some(dims) = self.image_dims() {
            self.view.zoom_to_cursor(factor, self.cursor, dims, &self.viewport);
            self.refresh(ctx);
        }
    }

    /// Keyboard zoom about the viewport center.
    pub fn zoom_centered(&mut self, ctx: &GpuContext, factor: f32) {
        if let Some(dims) = self.image_dims() {
            self.view.zoom_centered(factor, dims, &self.viewport);
            self.refresh(ctx);
        }
    }

    pub fn fit(&mut self, ctx: &GpuContext) {
        if let Some(dims) = self.image_dims() {
            self.view.fit_to_window(dims, &self.viewport);
            self.refresh(ctx);
        }
    }

    pub fn one_to_one(&mut self, ctx: &GpuContext) {
        self.view.one_to_one();
        if let Some(dims) = self.image_dims() {
            self.view.clamp_pan(dims, &self.viewport);
        }
        self.refresh(ctx);
    }

    /// Solo a channel; pressing the same channel again returns to RGB.
    pub fn toggle_channel(&mut self, ctx: &GpuContext, ch: Channel) {
        self.display.channel = if self.display.channel == ch { Channel::Rgb } else { ch };
        self.refresh(ctx);
    }

    pub fn set_channel(&mut self, ctx: &GpuContext, ch: Channel) {
        self.display.channel = ch;
        self.refresh(ctx);
    }

    /// Adjust exposure in stops (HDR sources; harmless no-op visually for LDR).
    pub fn adjust_exposure(&mut self, ctx: &GpuContext, delta: f32) {
        self.display.exposure = (self.display.exposure + delta).clamp(-16.0, 16.0);
        self.refresh(ctx);
    }

    pub fn toggle_tonemap(&mut self, ctx: &GpuContext) {
        self.display.tonemap = match self.display.tonemap {
            Tonemap::Reinhard => Tonemap::Aces,
            Tonemap::Aces => Tonemap::Reinhard,
        };
        self.refresh(ctx);
    }

    pub fn render(&mut self, ctx: &GpuContext) {
        use wgpu::CurrentSurfaceTexture as Cst;
        // wgpu 29: get_current_texture returns an enum, not a Result.
        let frame = match self.surface.get_current_texture() {
            Cst::Success(f) | Cst::Suboptimal(f) => f,
            // Lightweight surface loss: reconfigure and retry once. Full device-loss
            // rebuild + re-decode is Phase 5.
            Cst::Outdated | Cst::Lost => {
                self.surface.configure(&ctx.device, &self.config);
                match self.surface.get_current_texture() {
                    Cst::Success(f) | Cst::Suboptimal(f) => f,
                    _ => return,
                }
            }
            // Timeout / Occluded / Validation: skip this frame.
            _ => return,
        };

        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("image-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.05,
                            g: 0.05,
                            b: 0.06,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if let Some(bg) = &self.texture_bind_group {
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &self.uniform_bind_group, &[]);
                pass.set_bind_group(1, bg, &[]);
                pass.draw(0..4, 0..1);
            }
        }
        ctx.queue.submit([encoder.finish()]);
        frame.present();
    }
}

/// Narrow an interleaved f32 RGBA buffer to f16 bytes (native-endian, matching the other
/// texture uploads). HDR is displayed through f16, which is sampler-filterable on every
/// adapter; full f32 precision is kept in `current_image` for the inspector.
fn f32_rgba_to_f16_bytes(pixels: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(pixels.len() / 2);
    for c in pixels.chunks_exact(4) {
        let v = f32::from_ne_bytes([c[0], c[1], c[2], c[3]]);
        out.extend_from_slice(&f32_to_f16_bits(v).to_ne_bytes());
    }
    out
}

/// Minimal f32 → IEEE-754 half (f16) bit conversion. Subnormals flush to zero and the
/// mantissa truncates — ample for HDR *display* (exposure/tonemap run on these values; the
/// inspector reads the retained f32). Avoids pulling in the `half` crate.
fn f32_to_f16_bits(value: f32) -> u16 {
    let bits = value.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xff) as i32 - 127 + 15;
    let mant = bits & 0x007f_ffff;
    if exp <= 0 {
        // Underflow / subnormal → signed zero.
        sign
    } else if exp >= 0x1f {
        // Overflow / inf / NaN → signed inf (NaN payload dropped; fine for display).
        sign | 0x7c00
    } else {
        sign | ((exp as u16) << 10) | ((mant >> 13) as u16)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f16_round_numbers() {
        // 1.0 → 0x3C00, 2.0 → 0x4000, 0.5 → 0x3800, 0.0 → 0x0000, -1.0 → 0xBC00.
        assert_eq!(f32_to_f16_bits(1.0), 0x3C00);
        assert_eq!(f32_to_f16_bits(2.0), 0x4000);
        assert_eq!(f32_to_f16_bits(0.5), 0x3800);
        assert_eq!(f32_to_f16_bits(0.0), 0x0000);
        assert_eq!(f32_to_f16_bits(-1.0), 0xBC00);
    }

    #[test]
    fn f16_overflow_is_inf() {
        assert_eq!(f32_to_f16_bits(1.0e30) & 0x7fff, 0x7c00);
    }
}
