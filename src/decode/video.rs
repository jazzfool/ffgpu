use crate::{
    decode::{Clock, DecoderState, FrameQueue, PacketReceiver, PacketSender, PlayState},
    error::{Error, Result},
};
use crossbeam_channel::{Receiver, Sender};
use ffmpeg_next::{self as ffn, sys as ff};
use std::{
    mem::ManuallyDrop,
    pin::Pin,
    ptr::{NonNull, null_mut},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

struct DecoderData {
    hw_pixel_format: ff::AVPixelFormat,
    unsupported: Arc<AtomicBool>,
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
        decoder_data.unsupported.store(true, Ordering::Relaxed);
        ff::AVPixelFormat::AV_PIX_FMT_NONE
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoMetadata {
    pub index: usize,
    pub time_base: ffn::Rational,
    pub framerate: ffn::Rational,
    pub color_space: ffn::color::Space,
    pub width: u32,
    pub height: u32,
}

impl Default for VideoMetadata {
    fn default() -> Self {
        Self {
            index: usize::MAX,
            time_base: ffn::Rational::new(0, 1),
            framerate: ffn::Rational::new(0, 1),
            color_space: ffn::color::Space::Unspecified,
            width: 0,
            height: 0,
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
    pub metadata: VideoMetadata,
    pub unsupported: Arc<AtomicBool>,
    pub format_ctx: NonNull<ff::AVFormatContext>,
    pub device_type: ff::AVHWDeviceType,
    _decoder_data: Option<Pin<Box<DecoderData>>>,
}

impl Decoder {
    pub fn new(
        input: &mut ffn::format::context::Input,
        device_type: ff::AVHWDeviceType,
    ) -> Result<Self> {
        let video_stream = input
            .streams()
            .best(ffn::media::Type::Video)
            .ok_or(Error::InvalidStream)?;

        let video_stream_index = video_stream.index();

        let video_codec = video_stream.parameters().id();
        let decoder =
            ffn::decoder::find(video_codec).ok_or(Error::MissingCodec(video_codec.name()))?;

        let mut decoder_ctx = ffn::codec::Context::new_with_codec(decoder).decoder();
        unsafe { (*decoder_ctx.as_mut_ptr()).extra_hw_frames = 8 };
        decoder_ctx.set_parameters(video_stream.parameters())?;
        decoder_ctx.set_threading(ffn::threading::Config {
            kind: ffn::threading::Type::Frame,
            count: 0,
        });

        let unsupported = Arc::new(AtomicBool::new(false));
        let decoder_data = if device_type != ff::AVHWDeviceType::AV_HWDEVICE_TYPE_NONE {
            let mut hw_pixel_format = ff::AVPixelFormat::AV_PIX_FMT_NONE;
            for i in 0..16 {
                let config = unsafe {
                    ff::avcodec_get_hw_config(decoder.as_ptr(), i)
                        .as_ref()
                        .ok_or(Error::MissingCodec(video_codec.name()))?
                };
                if (config.methods & ff::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX as i32) != 0
                    && config.device_type == device_type
                {
                    hw_pixel_format = config.pix_fmt;
                    break;
                }
            }

            let mut decoder_data = Box::pin(DecoderData {
                hw_pixel_format,
                unsupported: unsupported.clone(),
            });
            unsafe {
                (*decoder_ctx.as_mut_ptr()).opaque = (&mut *decoder_data) as *mut _ as _;
                (*decoder_ctx.as_mut_ptr()).get_format = Some(get_hw_format);
            };

            let mut hwctx = null_mut();
            unsafe {
                ff::av_hwdevice_ctx_create(&mut hwctx, device_type, null_mut(), null_mut(), 0)
            };

            let hwctx = NonNull::new(hwctx).ok_or(Error::HardwareContext)?;
            unsafe {
                (*decoder_ctx.as_mut_ptr()).hw_device_ctx = ff::av_buffer_ref(hwctx.as_ptr());
            }

            Some(decoder_data)
        } else {
            None
        };

        let decoder_ctx = decoder_ctx.video()?;

        let width = decoder_ctx.width();
        let height = decoder_ctx.height();
        let color_space = decoder_ctx.color_space();

        let metadata = VideoMetadata {
            index: video_stream_index,
            time_base: video_stream.time_base(),
            framerate: video_stream.avg_frame_rate(),
            color_space,
            width: width,
            height: height,
        };

        Ok(Decoder {
            decoder: decoder_ctx,
            metadata,
            unsupported,
            format_ctx: NonNull::new(unsafe { input.as_mut_ptr() }).unwrap(),
            device_type,
            _decoder_data: decoder_data,
        })
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
            if self.decoder.device_type != ff::AVHWDeviceType::AV_HWDEVICE_TYPE_NONE
                && self.decoder.unsupported.load(Ordering::Relaxed)
            {
                log::error!("unsupported codec, falling back to software");
                let mut input = unsafe {
                    ManuallyDrop::new(ffn::format::context::Input::wrap(
                        self.decoder.format_ctx.as_ptr(),
                    ))
                };
                self.decoder = Decoder::new(&mut input, ff::AVHWDeviceType::AV_HWDEVICE_TYPE_NONE).unwrap(/* was already ok first time */);
            }

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
