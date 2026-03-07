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

pub(crate) struct PipelineCache {
    device: wgpu::Device,
    bg0_layout: wgpu::BindGroupLayout,
    pipeline_layout: wgpu::PipelineLayout,
    pipelines: BTreeMap<i32, wgpu::RenderPipeline>,
}

impl PipelineCache {
    pub fn new(device: wgpu::Device) -> Self {
        let bg0_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[&bg0_layout],
            immediate_size: 0,
        });

        PipelineCache {
            device,
            bg0_layout,
            pipeline_layout,
            pipelines: BTreeMap::new(),
        }
    }

    pub fn bind_group_layout(&self) -> &wgpu::BindGroupLayout {
        &self.bg0_layout
    }

    pub fn get(&mut self, color_space: ffn::color::Space) -> &wgpu::RenderPipeline {
        self.pipelines.entry(color_space as i32).or_insert_with(|| {
            let color_matrix = yuv_to_rgb_matrix(color_space);

            let shader_source = include_str!("copy.wgsl").replace(
                "$color_matrix",
                &color_matrix.map(|x| x.to_string()).join(","),
            );

            let shader = self
                .device
                .create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: None,
                    source: wgpu::ShaderSource::Wgsl(Cow::Owned(shader_source)),
                });

            self.device
                .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: None,
                    layout: Some(&self.pipeline_layout),
                    vertex: wgpu::VertexState {
                        module: &shader,
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
                })
        })
    }

    pub fn create_nv12_bind_group(
        device: &wgpu::Device,
        texture: &wgpu::Texture,
        layout: &wgpu::BindGroupLayout,
    ) -> wgpu::BindGroup {
        let view_y = texture.create_view(&wgpu::TextureViewDescriptor {
            label: None,
            format: None,
            dimension: None,
            usage: None,
            aspect: wgpu::TextureAspect::Plane0,
            base_mip_level: 0,
            mip_level_count: None,
            base_array_layer: 0,
            array_layer_count: None,
        });

        let view_uv = texture.create_view(&wgpu::TextureViewDescriptor {
            label: None,
            format: None,
            dimension: None,
            usage: None,
            aspect: wgpu::TextureAspect::Plane1,
            base_mip_level: 0,
            mip_level_count: None,
            base_array_layer: 0,
            array_layer_count: None,
        });

        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view_y),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&view_uv),
                },
            ],
        })
    }

    pub fn create_planar_bind_group(
        device: &wgpu::Device,
        y_texture: &wgpu::Texture,
        uv_texture: &wgpu::Texture,
        layout: &wgpu::BindGroupLayout,
    ) -> wgpu::BindGroup {
        let view_y = y_texture.create_view(&wgpu::TextureViewDescriptor {
            label: None,
            format: None,
            dimension: None,
            usage: None,
            aspect: wgpu::TextureAspect::All,
            base_mip_level: 0,
            mip_level_count: None,
            base_array_layer: 0,
            array_layer_count: None,
        });

        let view_uv = uv_texture.create_view(&wgpu::TextureViewDescriptor {
            label: None,
            format: None,
            dimension: None,
            usage: None,
            aspect: wgpu::TextureAspect::All,
            base_mip_level: 0,
            mip_level_count: None,
            base_array_layer: 0,
            array_layer_count: None,
        });

        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view_y),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&view_uv),
                },
            ],
        })
    }
}
