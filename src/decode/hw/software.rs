use crate::{
    Error, Result,
    context::{layout, pipeline_cache::PipelineCache},
    decode::hw::{FrameAdapter, FrameAdapterBuilder},
};
use ffmpeg_next::sys as ff;
use std::ptr::NonNull;

struct Texture {
    pixel_format: ff::AVPixelFormat,
    textures: layout::FrameDescriptor<wgpu::Texture>,
    bg0: wgpu::BindGroup,
}

pub struct SoftwareFrameAdapter {
    mapped_frame: NonNull<ff::AVFrame>,
    texture: Option<Texture>,
}

impl FrameAdapterBuilder for SoftwareFrameAdapter {
    unsafe fn new(_decoder: NonNull<ff::AVCodecContext>) -> Result<Self> {
        let mapped_frame = unsafe { NonNull::new(ff::av_frame_alloc()).expect("av_frame_alloc") };

        Ok(SoftwareFrameAdapter {
            mapped_frame,
            texture: None,
        })
    }

    fn supports_format(format: ff::AVPixelFormat) -> bool {
        layout::av_pixel_texture_format(format).is_some()
    }
}

impl FrameAdapter for SoftwareFrameAdapter {
    unsafe fn import_frame(
        &mut self,
        frame: NonNull<ff::AVFrame>,
        _instance: &wgpu::Instance,
        _adapter: &wgpu::Adapter,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _encoder: &mut wgpu::CommandEncoder,
        pipeline_cache: &mut PipelineCache,
    ) -> Result<()> {
        let frame = unsafe { frame.as_ref() };

        let texture = if let Some(texture) = &self.texture {
            texture
        } else {
            let pixel_format = if frame.hw_frames_ctx.is_null() {
                unsafe { std::mem::transmute(frame.format) }
            } else {
                unsafe {
                    let mut formats = std::ptr::null_mut();
                    ff::av_hwframe_transfer_get_formats(
                        frame.hw_frames_ctx,
                        ff::AVHWFrameTransferDirection::AV_HWFRAME_TRANSFER_DIRECTION_FROM,
                        &mut formats,
                        0,
                    );

                    let format = loop {
                        if *formats == ff::AVPixelFormat::AV_PIX_FMT_NONE
                            || layout::av_pixel_texture_format(*formats).is_some()
                        {
                            break *formats;
                        }
                        formats = formats.add(1);
                    };

                    ff::av_free(formats as _);
                    format
                }
            };

            let texture_format = layout::av_pixel_texture_format(pixel_format)
                .ok_or(Error::UnsupportedPixelFormat)?;

            let textures = layout::create_frame_textures(
                device,
                texture_format,
                frame.width as _,
                frame.height as _,
                wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
            )
            .ok_or(Error::UnsupportedPixelFormat)?;

            let bg0 = pipeline_cache.bind_frame_textures(
                &layout::FrameDescriptor {
                    planes: layout::create_frame_texture_views(
                        &textures.planes,
                        &Default::default(),
                    ),
                    depth: textures.depth,
                },
                frame.colorspace.into(),
            );

            let texture = Texture {
                pixel_format,
                textures,
                bg0,
            };

            self.texture.insert(texture)
        };

        let mapped_frame = if frame.hw_frames_ctx.is_null() {
            frame
        } else {
            let mapped_frame = unsafe { self.mapped_frame.as_mut() };
            mapped_frame.format = texture.pixel_format as _;
            let err = unsafe {
                ff::av_hwframe_map(mapped_frame as _, frame as _, ff::AV_HWFRAME_MAP_READ as _)
            };
            if err != 0 {
                unsafe {
                    let err = ff::av_hwframe_transfer_data(
                        mapped_frame as _,
                        frame as _,
                        ff::AVHWFrameTransferDirection::AV_HWFRAME_TRANSFER_DIRECTION_FROM as _,
                    );

                    if err != 0 {
                        return Err(Error::UnsupportedPixelFormat);
                    }
                }
            }
            mapped_frame
        };

        match &texture.textures.planes {
            layout::PlaneLayout::PackedYUV420([y, uv]) => {
                let y_stride = mapped_frame.linesize[0];
                let y_data = unsafe {
                    core::slice::from_raw_parts(
                        mapped_frame.data[0],
                        (y_stride * frame.height) as _,
                    )
                };

                let uv_stride = mapped_frame.linesize[1];
                let uv_data = unsafe {
                    core::slice::from_raw_parts(
                        mapped_frame.data[1],
                        (uv_stride * frame.height / 2) as _,
                    )
                };

                queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: y,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    y_data,
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(y_stride as _),
                        rows_per_image: None,
                    },
                    wgpu::Extent3d {
                        width: frame.width as _,
                        height: frame.height as _,
                        depth_or_array_layers: 1,
                    },
                );

                queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: uv,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    uv_data,
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(uv_stride as _),
                        rows_per_image: None,
                    },
                    wgpu::Extent3d {
                        width: frame.width as u32 / 2,
                        height: frame.height as u32 / 2,
                        depth_or_array_layers: 1,
                    },
                );
            }
            layout::PlaneLayout::YUV420([y, u, v]) => {
                let y_stride = mapped_frame.linesize[0];
                let y_data = unsafe {
                    core::slice::from_raw_parts(
                        mapped_frame.data[0],
                        (y_stride * frame.height) as _,
                    )
                };

                let u_stride = mapped_frame.linesize[1];
                let u_data = unsafe {
                    core::slice::from_raw_parts(
                        mapped_frame.data[1],
                        (u_stride * frame.height / 2) as _,
                    )
                };

                let v_stride = mapped_frame.linesize[2];
                let v_data = unsafe {
                    core::slice::from_raw_parts(
                        mapped_frame.data[2],
                        (v_stride * frame.height / 2) as _,
                    )
                };

                queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: y,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    y_data,
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(y_stride as _),
                        rows_per_image: None,
                    },
                    wgpu::Extent3d {
                        width: frame.width as _,
                        height: frame.height as _,
                        depth_or_array_layers: 1,
                    },
                );

                queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: u,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    u_data,
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(u_stride as _),
                        rows_per_image: None,
                    },
                    wgpu::Extent3d {
                        width: frame.width as u32 / 2,
                        height: frame.height as u32 / 2,
                        depth_or_array_layers: 1,
                    },
                );

                queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: v,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    v_data,
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(v_stride as _),
                        rows_per_image: None,
                    },
                    wgpu::Extent3d {
                        width: frame.width as u32 / 2,
                        height: frame.height as u32 / 2,
                        depth_or_array_layers: 1,
                    },
                );
            }
            layout::PlaneLayout::RGB(_) => todo!(),
        }

        unsafe { ff::av_frame_unref(self.mapped_frame.as_ptr()) };

        Ok(())
    }

    fn layout_identity(&self) -> Option<layout::FrameDescriptor<()>> {
        self.texture
            .as_ref()
            .map(|texture| texture.textures.as_identity())
    }

    fn bind_group(&self) -> Option<&wgpu::BindGroup> {
        self.texture.as_ref().map(|texture| &texture.bg0)
    }

    fn name(&self) -> &'static str {
        "Software"
    }
}
