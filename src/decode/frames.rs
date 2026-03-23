mod software;

#[cfg(target_os = "windows")]
mod d3d11va;
#[cfg(target_os = "linux")]
mod vaapi;
#[cfg(target_os = "macos")]
mod video_toolbox;

use crate::{
    Error, VideoMetadata,
    context::{layout, pipeline_cache::PipelineCache},
    error::Result,
};
use ffmpeg_next::{self as ffn, sys as ff};
use std::{
    ptr::NonNull,
    sync::{Arc, Mutex},
};

// needs to be separate from FrameAdapater to be dyn compatible
pub(crate) trait FrameAdapterBuilder: FrameAdapter + Sized {
    unsafe fn new(decoder: NonNull<ff::AVCodecContext>) -> Result<Self>;
    fn supports_format(format: ff::AVPixelFormat) -> bool;
}

pub(crate) trait FrameAdapter {
    unsafe fn import_frame(
        &mut self,
        frame: NonNull<ff::AVFrame>,
        instance: &wgpu::Instance,
        adapter: &wgpu::Adapter,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        pipeline_cache: &mut PipelineCache,
    ) -> Result<()>;
    fn layout_identity(&self) -> Option<layout::FrameDescriptor<()>>;
    fn bind_group(&self) -> Option<&wgpu::BindGroup>;
    fn name(&self) -> &'static str;
}

pub(crate) struct FrameDecoder {
    pub(crate) adapter: Option<Box<dyn FrameAdapter>>,
    pipeline_cache: Arc<Mutex<PipelineCache>>,
    last_pixel_format: ff::AVPixelFormat,
    texture: wgpu::Texture,
    texture_view: wgpu::TextureView,
}

impl FrameDecoder {
    pub fn new(
        device: &wgpu::Device,
        pipeline_cache: Arc<Mutex<PipelineCache>>,
        metadata: &VideoMetadata,
    ) -> Result<Self> {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d {
                width: metadata.width,
                height: metadata.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[wgpu::TextureFormat::Rgba8UnormSrgb],
        });
        let texture_view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        Ok(FrameDecoder {
            adapter: None,
            pipeline_cache,
            last_pixel_format: ff::AVPixelFormat::AV_PIX_FMT_NONE,
            texture,
            texture_view,
        })
    }

    pub unsafe fn decode_frame(
        &mut self,
        instance: &wgpu::Instance,
        adapter: &wgpu::Adapter,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        decoder: NonNull<ff::AVCodecContext>,
        frame: &ffn::Frame,
    ) -> Result<()> {
        let format =
            unsafe { std::mem::transmute::<_, ff::AVPixelFormat>((*frame.as_ptr()).format) };

        if format != self.last_pixel_format {
            self.last_pixel_format = format;
            self.adapter = None;
        }

        let frame_adapter = if let Some(frame_adapter) = self.adapter.as_mut() {
            frame_adapter
        } else {
            unsafe {
                let decoder = match format {
                    #[cfg(target_os = "windows")]
                    ff::AVPixelFormat::AV_PIX_FMT_D3D11 => {
                        Box::new(d3d11va::D3D11VAFrameAdapter::new(decoder)?) as _
                    }
                    format if software::SoftwareFrameAdapter::supports_format(format) => {
                        log::warn!("using CPU frame copies");
                        Box::new(software::SoftwareFrameAdapter::new(decoder)?) as _
                    }
                    _ => return Err(Error::UnsupportedPixelFormat),
                };
                self.adapter.insert(decoder)
            }
        };

        let mut pipeline_cache = self.pipeline_cache.lock().unwrap();

        unsafe {
            let res = frame_adapter.import_frame(
                NonNull::new_unchecked(frame.as_ptr() as *mut _),
                instance,
                adapter,
                device,
                queue,
                encoder,
                &mut *pipeline_cache,
            );
            if let Err(Error::UnsupportedBackend) = res {
                // don't worry... we can recover from this...
                log::error!("unsupported zero-copy WGPU backend");
                log::warn!("using CPU frame copies");
                self.adapter = Some(Box::new(software::SoftwareFrameAdapter::new(decoder)?));
                return Ok(());
            } else {
                res?
            }
        };

        drop(pipeline_cache);

        self.copy_to_rgb(encoder, unsafe { (*frame.as_ptr()).colorspace.into() });

        Ok(())
    }

    pub fn copy_to_rgb(&self, encoder: &mut wgpu::CommandEncoder, color_space: ffn::color::Space) {
        let Some(bg0) = self
            .adapter
            .as_ref()
            .and_then(|adapter| adapter.bind_group())
        else {
            return;
        };

        let Some(layout_identity) = self
            .adapter
            .as_ref()
            .and_then(|adapter| adapter.layout_identity())
        else {
            return;
        };

        let pipeline = self
            .pipeline_cache
            .lock()
            .unwrap()
            .get(layout_identity, color_space)
            .clone();

        let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: None,
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &self.texture_view,
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
        rpass.set_pipeline(&pipeline);
        rpass.set_bind_group(0, bg0, &[]);
        rpass.draw(0..3, 0..1);
    }

    pub fn texture(&self) -> &wgpu::Texture {
        &self.texture
    }
}
