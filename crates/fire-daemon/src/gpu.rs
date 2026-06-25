//! wgpu warm-up and per-window render state.
//!
//! [`GpuContext`] holds the expensive, window-independent objects (instance, adapter,
//! device, queue) created once at daemon startup — this is the cost the resident model
//! pays up front so per-open latency stays near zero (§5, §12). [`WindowState`] holds
//! the surface and render pipeline for the pooled window, created when the window is
//! (created hidden, also at startup).

use std::borrow::Cow;
use std::sync::Arc;

use fire_decode::{DecodedImage, PixelFormat};
use winit::window::Window;

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

/// Surface + pipeline for the pooled window, plus the current image's bind group.
pub struct WindowState {
    pub window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    /// `None` until the first image is uploaded; the render pass clears-only until then.
    texture_bind_group: Option<wgpu::BindGroup>,
    /// Monotonic per-window decode generation. Bumped on each open; a `DecodeDone`
    /// whose generation is older than this is stale and dropped (so a slow decode can't
    /// clobber a newer one).
    generation: u64,
    /// CPU copy of the currently displayed image, retained for the Phase-4 pixel
    /// inspector (#16). Device-loss recovery still re-decodes from the path (#11).
    current_image: Option<DecodedImage>,
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
        // Prefer an sRGB surface format so the hardware does linear→sRGB encode on
        // present (Phase 3 revisits the full color pipeline).
        let caps = surface.get_capabilities(&ctx.adapter);
        if let Some(srgb) = caps.formats.iter().copied().find(|f| f.is_srgb()) {
            config.format = srgb;
        }
        config.usage = wgpu::TextureUsages::RENDER_ATTACHMENT;
        surface.configure(&ctx.device, &config);

        let shader = ctx.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("blit-shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../shaders/blit.wgsl").into()),
        });

        let bind_group_layout =
            ctx.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
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

        let pipeline_layout =
            ctx.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("blit-pl"),
                // wgpu 29: bind_group_layouts is &[Option<&_>], and push constants were
                // renamed to "immediate data" (immediate_size, 0 = none).
                bind_group_layouts: &[Some(&bind_group_layout)],
                immediate_size: 0,
            });

        let pipeline = ctx.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("blit-pipeline"),
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
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let sampler = ctx.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("image-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            // wgpu 29: mipmap_filter is its own MipmapFilterMode, distinct from FilterMode.
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });

        Self {
            window,
            surface,
            config,
            pipeline,
            bind_group_layout,
            sampler,
            texture_bind_group: None,
            generation: 0,
            current_image: None,
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

    /// Drop the displayed image so the render pass clears-only — the placeholder shown
    /// while a freshly-opened file decodes (avoids flashing the previous file's pixels).
    pub fn clear_image(&mut self) {
        self.texture_bind_group = None;
        self.current_image = None;
    }

    /// Upload a decoded image to a GPU texture, bind it for rendering, and retain the
    /// CPU buffer for the inspector. Takes ownership: the decoded image lives on in
    /// `current_image` after upload (#16).
    ///
    /// The texture is always `Rgba8UnormSrgb` for now. Non-8-bit sources (16-bit, and
    /// float EXR/HDR) are CPU-converted to 8-bit sRGB here as a Phase-2 stopgap — for
    /// float that means a fixed Reinhard tonemap. Phase 3 replaces this with a native
    /// float texture plus in-shader exposure + tonemap (Reinhard/ACES) controls.
    pub fn set_image(&mut self, ctx: &GpuContext, img: DecodedImage) {
        let rgba8 = pixels_as_rgba8(&img);
        let size = wgpu::Extent3d {
            width: img.width,
            height: img.height,
            depth_or_array_layers: 1,
        };
        let texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("image-texture"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // sRGB texture: sampling decodes to linear, the sRGB surface re-encodes on
            // present → correct passthrough for sRGB sources.
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
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
            &rgba8,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4 * img.width),
                rows_per_image: Some(img.height),
            },
            size,
        );

        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("image-bind-group"),
            layout: &self.bind_group_layout,
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
    }

    pub fn resize(&mut self, ctx: &GpuContext, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&ctx.device, &self.config);
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
                label: Some("blit-pass"),
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
                pass.set_bind_group(0, bg, &[]);
                pass.draw(0..3, 0..1);
            }
        }
        ctx.queue.submit([encoder.finish()]);
        frame.present();
    }
}

/// Convert a decoded image to 8-bit sRGB RGBA bytes for the `Rgba8UnormSrgb` texture.
/// 8-bit sources pass through; 16-bit are scaled; float (EXR/HDR) get a fixed Reinhard
/// tonemap + sRGB encode (Phase-2 stopgap; Phase 3 does this in-shader on a float tex).
fn pixels_as_rgba8(img: &DecodedImage) -> Cow<'_, [u8]> {
    match img.format {
        PixelFormat::Rgba8Unorm => Cow::Borrowed(&img.pixels),
        PixelFormat::Rgba16Unorm => {
            let out = img
                .pixels
                .chunks_exact(2)
                .map(|c| (u16::from_ne_bytes([c[0], c[1]]) >> 8) as u8)
                .collect();
            Cow::Owned(out)
        }
        PixelFormat::Rgba32Float => {
            let mut out = Vec::with_capacity((img.width * img.height * 4) as usize);
            for (i, c) in img.pixels.chunks_exact(4).enumerate() {
                let v = f32::from_ne_bytes([c[0], c[1], c[2], c[3]]);
                let is_alpha = i % 4 == 3;
                out.push(if is_alpha {
                    (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
                } else {
                    let tonemapped = v.max(0.0) / (1.0 + v.max(0.0)); // Reinhard
                    (srgb_encode(tonemapped) * 255.0 + 0.5) as u8
                });
            }
            Cow::Owned(out)
        }
        PixelFormat::Rgba16Float => {
            // Not produced by the current decode backends; render mid-gray if it occurs.
            Cow::Owned(vec![0x80; (img.width * img.height * 4) as usize])
        }
    }
}

/// Linear → sRGB transfer function for a single [0,1] channel.
fn srgb_encode(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    if x <= 0.0031308 {
        12.92 * x
    } else {
        1.055 * x.powf(1.0 / 2.4) - 0.055
    }
}
