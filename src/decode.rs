#[cfg(target_os = "windows")]
mod d3d11va;
mod vaapi;
#[cfg(target_os = "macos")]
mod video_toolbox;

use ffmpeg_sys_next as ff;
use std::{
    ffi::CString,
    ops::RangeInclusive,
    pin::Pin,
    ptr::{NonNull, null, null_mut},
};

use crate::context::pipeline_cache::PipelineCache;

pub(crate) trait HardwareDecoder {
    const DEVICE_TYPE: ff::AVHWDeviceType;
    const AVUTIL_VERSION: RangeInclusive<u32>;

    unsafe fn new(hwctx: *mut ff::AVBufferRef) -> Self;
    unsafe fn import_frame(
        &mut self,
        frame: NonNull<ff::AVFrame>,
        adapter: &wgpu::Adapter,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        layout: &wgpu::BindGroupLayout,
    ) -> Option<&wgpu::BindGroup>;
}

pub(crate) const fn av_version(major: u8, minor: u8, rev: u8) -> u32 {
    (rev as u32) | ((minor as u32) << 8) | ((major as u32) << 16)
}

#[cfg(target_os = "windows")]
type NativeDecoder = d3d11va::D3D11VAHardwareDecoder;

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
    unsafe fn new(
        hwctx: *mut ff::AVBufferRef,
        device: &wgpu::Device,
        pipeline_cache: &mut PipelineCache,
        color_space: ff::AVColorSpace,
        width: u32,
        height: u32,
    ) -> Self {
        let hwdec = unsafe { NativeDecoder::new(hwctx) };

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

        FrameDecoder {
            hwdec,
            texture,
            texture_view,
            bg0_layout,
            pipeline,
        }
    }

    pub unsafe fn decode_native_frame(
        &mut self,
        adapter: &wgpu::Adapter,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        frame: NonNull<ff::AVFrame>,
    ) {
        let bg0 = unsafe {
            self.hwdec
                .import_frame(frame, adapter, device, queue, &self.bg0_layout)
        };
        let Some(bg0) = bg0 else { return };

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

pub(crate) struct QueryInfo {
    pub time_base: ff::AVRational,
    pub framerate: ff::AVRational,
    pub width: u32,
    pub height: u32,
}

struct DecoderData {
    hw_pixel_format: ff::AVPixelFormat,
}

pub(crate) struct Decoder {
    pub format_ctx: *mut ff::AVFormatContext,
    pub decoder_ctx: *mut ff::AVCodecContext,
    pub hwctx: *mut ff::AVBufferRef,
    pub video_stream: *mut ff::AVStream,
    pub video_stream_idx: i32,
    _decoder_data: Pin<Box<DecoderData>>,
}

impl Decoder {
    pub unsafe fn new(
        device: &wgpu::Device,
        pipeline_cache: &mut PipelineCache,
    ) -> (Self, FrameDecoder) {
        unsafe {
            assert!(
                NativeDecoder::AVUTIL_VERSION.contains(&ff::avutil_version()),
                "unsupported ffmpeg version"
            );

            ff::avdevice_register_all();

            let mut format_ctx = ff::avformat_alloc_context();
            ff::avformat_open_input(
                &mut format_ctx,
                CString::new("test.mp4").unwrap().as_ptr(),
                null_mut(),
                null_mut(),
            );
            ff::avformat_find_stream_info(format_ctx, null_mut());

            let mut decoder = null();
            let video_stream_idx = ff::av_find_best_stream(
                format_ctx,
                ff::AVMediaType::AVMEDIA_TYPE_VIDEO,
                -1,
                -1,
                &mut decoder,
                0,
            );

            let mut hw_pixel_format = ff::AVPixelFormat::AV_PIX_FMT_NONE;
            for i in 0..16 {
                let config = ff::avcodec_get_hw_config(decoder, i);
                if config.is_null() {
                    panic!("unsupported decoder");
                }
                if ((*config).methods & ff::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX as i32) != 0
                    && (*config).device_type == NativeDecoder::DEVICE_TYPE
                {
                    hw_pixel_format = (*config).pix_fmt;
                    break;
                }
            }

            let decoder_ctx = ff::avcodec_alloc_context3(decoder);

            let video_stream = *(*format_ctx).streams.add(video_stream_idx as _);
            ff::avcodec_parameters_to_context(decoder_ctx, (*video_stream).codecpar);

            let width = (*(*video_stream).codecpar).width;
            let height = (*(*video_stream).codecpar).height;
            let color_space = (*(*video_stream).codecpar).color_space;

            // TODO: set opaque ptr to data we need to keep in callbacks
            // (*decoder_ctx).opaque

            let mut decoder_data = Box::pin(DecoderData { hw_pixel_format });
            (*decoder_ctx).opaque = (&mut *decoder_data) as *mut _ as _;
            (*decoder_ctx).get_format = Some(get_hw_format);

            let mut hwctx = null_mut();
            ff::av_hwdevice_ctx_create(
                &mut hwctx,
                NativeDecoder::DEVICE_TYPE,
                null_mut(),
                null_mut(),
                0,
            );

            (*decoder_ctx).hw_device_ctx = ff::av_buffer_ref(hwctx);

            ff::avcodec_open2(decoder_ctx, decoder, null_mut());

            let frame_decoder = FrameDecoder::new(
                hwctx,
                device,
                pipeline_cache,
                color_space,
                width as _,
                height as _,
            );

            (
                Decoder {
                    format_ctx,
                    decoder_ctx,
                    hwctx,
                    video_stream,
                    video_stream_idx,
                    _decoder_data: decoder_data,
                },
                frame_decoder,
            )
        }
    }

    pub unsafe fn query_info(&self) -> QueryInfo {
        let stream = unsafe { self.video_stream.as_ref().unwrap() };
        let codecpar = unsafe { stream.codecpar.as_ref().unwrap() };
        QueryInfo {
            time_base: stream.time_base,
            framerate: unsafe {
                ff::av_guess_frame_rate(self.format_ctx, self.video_stream, null_mut())
            },
            width: codecpar.width as _,
            height: codecpar.height as _,
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe {
            ff::av_buffer_unref(&mut self.hwctx);
            ff::avcodec_free_context(&mut self.decoder_ctx);
            ff::avformat_close_input(&mut self.format_ctx);
        }
    }
}

unsafe impl Send for Decoder {}
unsafe impl Sync for Decoder {}
