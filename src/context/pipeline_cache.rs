use crate::context::layout;
use ffmpeg_next::{self as ffn, sys as ff};
use std::{borrow::Cow, collections::BTreeMap};

fn yuv_to_rgb_matrix(color_space: ffn::color::Space) -> [f32; 9] {
    assert!(
        matches!(
            color_space,
            ffn::color::Space::BT709
                | ffn::color::Space::FCC
                | ffn::color::Space::BT470BG
                | ffn::color::Space::BT2020CL
        ),
        "unsupported video color space {:#?}",
        color_space
    );
    let coeffs =
        unsafe { core::slice::from_raw_parts(ff::sws_getCoefficients(color_space as i32), 4) };
    // see libswscale/yuv2rgb.c
    let scale = 224.0 / (255.0 * 65536.0);
    [
        1.0,
        0.0,
        coeffs[0] as f32 * scale,
        1.0,
        -coeffs[2] as f32 * scale,
        -coeffs[3] as f32 * scale,
        1.0,
        coeffs[1] as f32 * scale,
        0.0,
    ]
}

struct CachedPipeline {
    bg0_layout: wgpu::BindGroupLayout,
    pipeline: wgpu::RenderPipeline,
}

pub(crate) struct PipelineCache {
    device: wgpu::Device,
    vertex_shader: wgpu::ShaderModule,
    pipelines: BTreeMap<(layout::FrameDescriptor<()>, i32), CachedPipeline>,
}

impl PipelineCache {
    pub fn new(device: wgpu::Device) -> Self {
        let vertex_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: None,
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(include_str!("fullscreen.wgsl"))),
        });

        PipelineCache {
            device,
            vertex_shader,
            pipelines: BTreeMap::new(),
        }
    }

    fn get_cached(
        &mut self,
        identity: layout::FrameDescriptor<()>,
        color_space: ffn::color::Space,
    ) -> &CachedPipeline {
        let color_space = if color_space == ffn::color::Space::Unspecified {
            ffn::color::Space::BT709
        } else {
            color_space
        };

        self.pipelines
            .entry((identity, color_space as i32))
            .or_insert_with(|| {
                let float_view = |i| wgpu::BindGroupLayoutEntry {
                    binding: i,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                };

                let layout_entries: &[_] = match identity.planes {
                    layout::PlaneLayout::PackedYUV420(_) => &[float_view(0), float_view(1)],
                    layout::PlaneLayout::YUV420(_) => {
                        &[float_view(0), float_view(1), float_view(2)]
                    }
                    layout::PlaneLayout::RGB(_) => &[float_view(0)],
                };

                let bg0_layout =
                    self.device
                        .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                            label: None,
                            entries: layout_entries,
                        });

                let layout = self
                    .device
                    .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                        label: None,
                        bind_group_layouts: &[&bg0_layout],
                        immediate_size: 0,
                    });

                let color_matrix = yuv_to_rgb_matrix(color_space);

                let shader_source = match identity.planes {
                    layout::PlaneLayout::PackedYUV420(_) => include_str!("yuv420_packed.wgsl"),
                    layout::PlaneLayout::YUV420(_) => include_str!("yuv420.wgsl"),
                    layout::PlaneLayout::RGB(_) => todo!(),
                };
                let shader_source = shader_source
                    .replace(
                        "$color_matrix",
                        &color_matrix.map(|x| x.to_string()).join(","),
                    )
                    .replace(
                        "$scale",
                        &match identity.depth {
                            layout::Depth::D8 => 1.0,
                            layout::Depth::D10 => (1 << 16) as f32 / (1 << 10) as f32,
                            layout::Depth::D12 => (1 << 16) as f32 / (1 << 12) as f32,
                            layout::Depth::D16 => 1.0,
                        }
                        .to_string(),
                    );

                let shader = self
                    .device
                    .create_shader_module(wgpu::ShaderModuleDescriptor {
                        label: None,
                        source: wgpu::ShaderSource::Wgsl(Cow::Owned(shader_source)),
                    });

                let pipeline =
                    self.device
                        .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                            label: None,
                            layout: Some(&layout),
                            vertex: wgpu::VertexState {
                                module: &self.vertex_shader,
                                entry_point: Some("vs_main"),
                                buffers: &[],
                                compilation_options: Default::default(),
                            },
                            fragment: Some(wgpu::FragmentState {
                                module: &shader,
                                entry_point: Some("fs_main"),
                                compilation_options: Default::default(),
                                targets: &[Some(wgpu::ColorTargetState {
                                    format: wgpu::TextureFormat::Rgba8Unorm,
                                    blend: None,
                                    write_mask: wgpu::ColorWrites::all(),
                                })],
                            }),
                            primitive: wgpu::PrimitiveState::default(),
                            depth_stencil: None,
                            multisample: wgpu::MultisampleState::default(),
                            multiview_mask: None,
                            cache: None,
                        });

                CachedPipeline {
                    bg0_layout,
                    pipeline,
                }
            })
    }

    pub fn get(
        &mut self,
        identity: layout::FrameDescriptor<()>,
        color_space: ffn::color::Space,
    ) -> &wgpu::RenderPipeline {
        &self.get_cached(identity, color_space).pipeline
    }

    pub fn bind_frame_textures(
        &mut self,
        textures: &layout::FrameDescriptor<wgpu::TextureView>,
        color_space: ffn::color::Space,
    ) -> wgpu::BindGroup {
        let bg0_layout = self
            .get_cached(textures.as_identity(), color_space)
            .bg0_layout
            .clone();

        match &textures.planes {
            layout::PlaneLayout::PackedYUV420([y, uv]) => {
                self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: None,
                    layout: &bg0_layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(y),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(uv),
                        },
                    ],
                })
            }
            layout::PlaneLayout::YUV420([y, u, v]) => {
                self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: None,
                    layout: &bg0_layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(y),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(u),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: wgpu::BindingResource::TextureView(v),
                        },
                    ],
                })
            }
            layout::PlaneLayout::RGB(_) => todo!(),
        }
    }
}
