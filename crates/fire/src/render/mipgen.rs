//! Mip-chain generation. wgpu has no `GenerateMips`; the image texture's pyramid is built with a
//! small blit pass instead — one fullscreen triangle per level, sampling the level above through
//! a linear sampler, which for a 2:1 reduction is a box filter. One pipeline per texture format,
//! created on first use and shared by every window.

use std::cell::RefCell;
use std::collections::HashMap;

/// The blit shader, its bind-group layout and samplers, and the per-format pipeline cache.
pub struct MipGen {
    shader: wgpu::ShaderModule,
    layout: wgpu::BindGroupLayout,
    pipeline_layout: wgpu::PipelineLayout,
    samp_linear: wgpu::Sampler,
    samp_nearest: wgpu::Sampler,
    pipelines: RefCell<HashMap<wgpu::TextureFormat, wgpu::RenderPipeline>>,
}

impl MipGen {
    pub fn new(device: &wgpu::Device) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("fire mipgen"),
            source: wgpu::ShaderSource::Wgsl(include_str!("mipgen.wgsl").into()),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("fire mipgen"),
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
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("fire mipgen"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let sampler = |filter: wgpu::FilterMode| {
            device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("fire mipgen"),
                address_mode_u: wgpu::AddressMode::ClampToEdge,
                address_mode_v: wgpu::AddressMode::ClampToEdge,
                address_mode_w: wgpu::AddressMode::ClampToEdge,
                mag_filter: filter,
                min_filter: filter,
                mipmap_filter: wgpu::MipmapFilterMode::Nearest,
                ..Default::default()
            })
        };
        Self {
            shader,
            layout,
            pipeline_layout,
            samp_linear: sampler(wgpu::FilterMode::Linear),
            samp_nearest: sampler(wgpu::FilterMode::Nearest),
            pipelines: RefCell::new(HashMap::new()),
        }
    }

    /// Fill levels `1..levels` of `texture` from level 0. `filterable` says whether `format` may be
    /// sampled with a linear filter on this device (32-bit float needs a feature); a
    /// non-filterable format is reduced with a nearest tap instead — coarser, but never a
    /// validation error.
    pub fn generate(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        texture: &wgpu::Texture,
        format: wgpu::TextureFormat,
        levels: u32,
        filterable: bool,
    ) {
        if levels <= 1 {
            return;
        }
        let pipeline = self.pipeline_for(device, format);
        let sampler = if filterable {
            &self.samp_linear
        } else {
            &self.samp_nearest
        };
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("fire mipgen"),
        });
        for level in 1..levels {
            let view_of = |base: u32| {
                texture.create_view(&wgpu::TextureViewDescriptor {
                    label: Some("fire mipgen level"),
                    base_mip_level: base,
                    mip_level_count: Some(1),
                    ..Default::default()
                })
            };
            let src = view_of(level - 1);
            let dst = view_of(level);
            let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("fire mipgen"),
                layout: &self.layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&src),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(sampler),
                    },
                ],
            });
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("fire mipgen"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &dst,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&pipeline);
            pass.set_bind_group(0, &bind, &[]);
            pass.draw(0..3, 0..1);
        }
        queue.submit([encoder.finish()]);
    }

    fn pipeline_for(
        &self,
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
    ) -> wgpu::RenderPipeline {
        if let Some(p) = self.pipelines.borrow().get(&format) {
            return p.clone();
        }
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("fire mipgen"),
            layout: Some(&self.pipeline_layout),
            vertex: wgpu::VertexState {
                module: &self.shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &self.shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        self.pipelines.borrow_mut().insert(format, pipeline.clone());
        pipeline
    }
}
