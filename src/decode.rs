#[cfg(target_os = "windows")]
mod d3d11va;
#[cfg(target_os = "linux")]
mod vaapi;
#[cfg(target_os = "macos")]
mod video_toolbox;

use crate::{
    context::pipeline_cache::PipelineCache,
    error::{Error, Result},
};
use ffmpeg_next::{self as ffn, sys as ff};
use std::{
    path::Path,
    pin::Pin,
    ptr::{NonNull, null_mut},
    sync::Mutex,
    time::Duration,
};

pub(crate) trait HardwareDecoder: Sized {
    const DEVICE_TYPE: ff::AVHWDeviceType;

    unsafe fn new(hwctx: NonNull<ff::AVBufferRef>) -> Result<Self>;
    unsafe fn import_frame(
        &mut self,
        frame: NonNull<ff::AVFrame>,
        instance: &wgpu::Instance,
        adapter: &wgpu::Adapter,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        layout: &wgpu::BindGroupLayout,
    ) -> Result<()>;
    fn bind_group(&self) -> Option<&wgpu::BindGroup>;
}

#[cfg(target_os = "windows")]
type NativeDecoder = d3d11va::D3D11VAHardwareDecoder;

#[cfg(target_os = "linux")]
type NativeDecoder = vaapi::VAAPIHardwareDecoder;

#[cfg(target_os = "macos")]
type NativeDecoder = video_toolbox::VideoToolboxHardwareDecoder;

unsafe extern "C" fn get_hw_format(
    decoder_ctx: *mut ff::AVCodecContext,
    mut px_fmts: *const ff::AVPixelFormat,
) -> ff::AVPixelFormat {
    unsafe {
        let decoder_data = ((*decoder_ctx).opaque as *mut DecoderData)
            .as_mut()
            .unwrap();
        while (*px_fmts) != ff::AVPixelFormat::AV_PIX_FMT_NONE {
            if (*px_fmts) == decoder_data.hw_pixel_format {
                return *px_fmts;
            }
            px_fmts = px_fmts.add(1);
        }
        ff::AVPixelFormat::AV_PIX_FMT_NONE
    }
}

pub struct FrameDecoder {
    hwdec: NativeDecoder,
    texture: wgpu::Texture,
    texture_view: wgpu::TextureView,
    bg0_layout: wgpu::BindGroupLayout,
    pipeline: wgpu::RenderPipeline,
}

impl FrameDecoder {
    fn new(
        hwctx: NonNull<ff::AVBufferRef>,
        device: &wgpu::Device,
        pipeline_cache: &mut PipelineCache,
        color_space: ffn::color::Space,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        let hwdec = unsafe { NativeDecoder::new(hwctx)? };

        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d {
                width: width,
                height: height,
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

        let bg0_layout = pipeline_cache.bind_group_layout().clone();
        let pipeline = pipeline_cache.get(color_space).clone();

        Ok(FrameDecoder {
            hwdec,
            texture,
            texture_view,
            bg0_layout,
            pipeline,
        })
    }

    pub unsafe fn decode_native_frame(
        &mut self,
        instance: &wgpu::Instance,
        adapter: &wgpu::Adapter,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        frame: &ffn::Frame,
    ) -> Result<()> {
        unsafe {
            self.hwdec.import_frame(
                NonNull::new_unchecked(frame.as_ptr() as *mut _),
                instance,
                adapter,
                device,
                queue,
                encoder,
                &self.bg0_layout,
            )?
        };
        self.copy_to_rgb(encoder);
        Ok(())
    }

    pub fn copy_to_rgb(&self, encoder: &mut wgpu::CommandEncoder) {
        let Some(bg0) = self.hwdec.bind_group() else {
            return;
        };

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
        rpass.set_pipeline(&self.pipeline);
        rpass.set_bind_group(0, bg0, &[]);
        rpass.draw(0..3, 0..1);
    }

    pub fn texture(&self) -> &wgpu::Texture {
        &self.texture
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct QueryInfo {
    pub time_base: ffn::Rational,
    pub framerate: ffn::Rational,
    pub width: u32,
    pub height: u32,
    pub duration: Duration,
}

struct DecoderData {
    hw_pixel_format: ff::AVPixelFormat,
}

pub(crate) struct Decoder {
    pub format_ctx: Mutex<ffn::format::context::Input>,
    pub decoder_ctx: Mutex<ffn::decoder::Video>,
    pub hwctx: NonNull<ff::AVBufferRef>,
    pub video_stream_index: usize,
    pub query_info: QueryInfo,
    _decoder_data: Pin<Box<DecoderData>>,
}

impl Decoder {
    pub fn new<P>(
        device: &wgpu::Device,
        pipeline_cache: &mut PipelineCache,
        path: &P,
    ) -> Result<(Self, FrameDecoder)>
    where
        P: AsRef<Path> + ?Sized,
    {
        ffn::init()?;

        let format_ctx = ffn::format::input(path)?;

        let video_stream = format_ctx
            .streams()
            .best(ffn::media::Type::Video)
            .ok_or(Error::InvalidStream)?;

        let video_stream_index = video_stream.index();

        let video_codec = video_stream.parameters().id();
        let decoder =
            ffn::decoder::find(video_codec).ok_or(Error::MissingCodec(video_codec.name()))?;

        let mut hw_pixel_format = ff::AVPixelFormat::AV_PIX_FMT_NONE;
        for i in 0..16 {
            let config = unsafe {
                ff::avcodec_get_hw_config(decoder.as_ptr(), i)
                    .as_ref()
                    .ok_or(Error::MissingCodec(video_codec.name()))?
            };
            if (config.methods & ff::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX as i32) != 0
                && config.device_type == NativeDecoder::DEVICE_TYPE
            {
                hw_pixel_format = config.pix_fmt;
                break;
            }
        }

        let mut decoder_ctx = ffn::codec::Context::new_with_codec(decoder).decoder();
        decoder_ctx.set_parameters(video_stream.parameters())?;
        decoder_ctx.set_threading(ffn::threading::Config {
            kind: ffn::threading::Type::Frame,
            count: 0,
        });

        let mut decoder_data = Box::pin(DecoderData { hw_pixel_format });
        unsafe {
            (*decoder_ctx.as_mut_ptr()).opaque = (&mut *decoder_data) as *mut _ as _;
            (*decoder_ctx.as_mut_ptr()).get_format = Some(get_hw_format);
        };

        let mut hwctx = null_mut();
        unsafe {
            ff::av_hwdevice_ctx_create(
                &mut hwctx,
                NativeDecoder::DEVICE_TYPE,
                null_mut(),
                null_mut(),
                0,
            )
        };

        let hwctx = NonNull::new(hwctx).ok_or(Error::HardwareContext)?;
        unsafe {
            (*decoder_ctx.as_mut_ptr()).hw_device_ctx = ff::av_buffer_ref(hwctx.as_ptr());
        }

        let decoder_ctx = decoder_ctx.video()?;

        let width = decoder_ctx.width();
        let height = decoder_ctx.height();
        let color_space = decoder_ctx.color_space();

        let frame_decoder = FrameDecoder::new(
            hwctx,
            device,
            pipeline_cache,
            color_space,
            width as _,
            height as _,
        )?;

        let query_info = QueryInfo {
            time_base: video_stream.time_base(),
            framerate: video_stream.avg_frame_rate(),
            width: width,
            height: height,
            duration: Duration::from_secs_f64(
                video_stream.duration() as f64 * video_stream.time_base().0 as f64
                    / video_stream.time_base().1 as f64,
            ),
        };

        Ok((
            Decoder {
                format_ctx: Mutex::new(format_ctx),
                decoder_ctx: Mutex::new(decoder_ctx),
                hwctx,
                video_stream_index,
                query_info,
                _decoder_data: decoder_data,
            },
            frame_decoder,
        ))
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe {
            ff::av_buffer_unref(&mut self.hwctx.as_ptr());
        }
    }
}

unsafe impl Send for Decoder {}
unsafe impl Sync for Decoder {}
