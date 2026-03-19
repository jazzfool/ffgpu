use crate::{
    context::pipeline_cache::PipelineCache,
    decode::{
        Clock, DecoderState, FrameQueue, PacketReceiver, PacketSender, PlayState,
        hw::{HardwareDecoder, NativeDecoder},
        read::Input,
    },
    error::{Error, Result},
};
use crossbeam_channel::{Receiver, Sender};
use ffmpeg_next::{self as ffn, sys as ff};
use std::{
    pin::Pin,
    ptr::{NonNull, null_mut},
    sync::{Arc, atomic::Ordering},
    thread::JoinHandle,
    time::Duration,
};

struct DecoderData {
    hw_pixel_format: ff::AVPixelFormat,
}

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
    pub(crate) hwdec: NativeDecoder,
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
pub struct VideoMetadata {
    pub index: usize,
    pub time_base: ffn::Rational,
    pub framerate: ffn::Rational,
    pub width: u32,
    pub height: u32,
    pub duration: Duration,
}

impl Default for VideoMetadata {
    fn default() -> Self {
        Self {
            index: usize::MAX,
            time_base: ffn::Rational::new(0, 1),
            framerate: ffn::Rational::new(0, 1),
            width: 0,
            height: 0,
            duration: Duration::ZERO,
        }
    }
}

pub(crate) struct VideoStream {
    pub metadata: VideoMetadata,
    pub messages: Sender<Message>,
    pub packets: PacketSender,
    pub frames: FrameQueue,
}

pub(crate) struct Decoder {
    pub decoder: ffn::decoder::Video,
    pub hwctx: NonNull<ff::AVBufferRef>,
    pub metadata: VideoMetadata,
    _decoder_data: Pin<Box<DecoderData>>,
}

impl Decoder {
    pub fn new(
        input: &mut Input,
        device: &wgpu::Device,
        pipeline_cache: &mut PipelineCache,
    ) -> Result<(Self, FrameDecoder)> {
        let video_stream = input
            .format_ctx
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

        let metadata = VideoMetadata {
            index: video_stream_index,
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
                decoder: decoder_ctx,
                hwctx,
                metadata,
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

pub(crate) enum Message {
    SkipToTimestamp(i64),
}

pub(crate) struct VideoThread {
    decoder: Decoder,
    state: Arc<DecoderState>,
    video_rx: PacketReceiver,
    frame_queue: FrameQueue,
    messages: Receiver<Message>,
    clock: Arc<Clock>,
    master_clock: Arc<Clock>,
}

impl VideoThread {
    pub fn new(
        decoder: Decoder,
        pbs: Arc<DecoderState>,
        video_rx: PacketReceiver,
        frame_queue: FrameQueue,
        messages: Receiver<Message>,
        clock: Arc<Clock>,
        master_clock: Arc<Clock>,
    ) -> Self {
        VideoThread {
            decoder,
            state: pbs,
            video_rx,
            frame_queue,
            messages,
            clock,
            master_clock,
        }
    }

    fn run_thread(&mut self) {
        let mut packet_serial = 0;

        let mut frame = unsafe { ffn::Frame::empty() };

        let mut skip_to_ts = None;

        'exit: while self.state.alive.load(Ordering::Relaxed) {
            while let Ok(message) = self.messages.try_recv() {
                match message {
                    Message::SkipToTimestamp(ts) => {
                        skip_to_ts = Some(ts);
                    }
                }
            }

            let mut prev_frame = None;

            while self.state.alive.load(Ordering::Relaxed) {
                if self.frame_queue.free_rx.is_empty() {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }

                let frame = match self.decoder.decoder.receive_frame(&mut frame) {
                    Ok(_) => {
                        if let Some(pts) = frame.pts() {
                            let av_pts = unsafe {
                                ff::av_rescale_q(
                                    pts,
                                    self.decoder.metadata.time_base.into(),
                                    ff::AV_TIME_BASE_Q,
                                )
                            };

                            if let Some(ts) = skip_to_ts
                                && av_pts < ts
                            {
                                // discard frame
                                // but keep a ref in case next receive is EOF
                                // in which case TS is past last frame
                                let mut frame_ref = unsafe { ffn::Frame::empty() };
                                unsafe {
                                    ff::av_frame_move_ref(
                                        frame_ref.as_mut_ptr(),
                                        frame.as_mut_ptr(),
                                    )
                                };
                                prev_frame = Some(frame_ref);
                                continue;
                            } else {
                                prev_frame = None;
                            }

                            let pts_sec = pts as f64 * f64::from(self.decoder.metadata.time_base);
                            if let Some(master) = self.master_clock.get() {
                                // drop early frame
                                let diff = pts_sec - master;
                                if diff.abs() < Clock::NO_SYNC_THRESHOLD
                                    && diff < 0.
                                    && packet_serial == self.clock.serial.load(Ordering::Relaxed)
                                    && !self.video_rx.rx.is_empty()
                                {
                                    continue;
                                }
                            }
                        }
                        Some(&mut frame)
                    }
                    Err(ffn::Error::Eof) => {
                        if let Some(mut prev_frame) = prev_frame.take() {
                            unsafe {
                                ff::av_frame_move_ref(frame.as_mut_ptr(), prev_frame.as_mut_ptr())
                            };
                            Some(&mut frame)
                        } else {
                            self.decoder.decoder.flush();
                            break;
                        }
                    }
                    Err(ffn::Error::Other { errno: ff::EAGAIN }) => {
                        break;
                    }
                    _ => None,
                };

                if let Some(frame) = frame {
                    prev_frame = None;

                    let mut step = false;
                    if let Some(pts) = frame.pts() {
                        self.state.current_pts.store(pts, Ordering::Relaxed);
                        if skip_to_ts.is_some() {
                            step = self.state.play_state() == PlayState::Paused;
                        }
                    }
                    skip_to_ts = None;

                    self.frame_queue.send(frame, packet_serial);
                    if step {
                        self.state
                            .play_state
                            .store(PlayState::Step as _, Ordering::Relaxed);
                    }
                }
            }

            let packet = loop {
                if !self.state.alive.load(Ordering::Relaxed) {
                    break 'exit;
                }

                let Some(packet) = self.video_rx.receive() else {
                    continue;
                };

                if packet_serial != packet.serial {
                    self.decoder.decoder.flush();
                    packet_serial = packet.serial;
                }

                if packet_serial == self.video_rx.metadata.serial.load(Ordering::Relaxed) {
                    break packet;
                }
            };

            if let Err(error) = self.decoder.decoder.send_packet(&packet.packet) {
                log::error!("failed to send packet: {}", error);
            }
        }
    }

    pub fn run(mut self) -> JoinHandle<()> {
        std::thread::spawn(move || {
            self.run_thread();
        })
    }
}
