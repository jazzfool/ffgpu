use crate::{
    decode::{
        Clock, DecoderState, Frame, FrameQueue, PacketQueueMetadata, PacketReceiver, PacketSender,
        PlayState, read::Input,
    },
    error::{Error, Result},
};
use atomic_float::AtomicF32;
use crossbeam_channel::{Receiver, Sender};
use ffmpeg_next::{self as ffn, sys as ff};
use std::{
    mem::ManuallyDrop,
    sync::{Arc, atomic::Ordering},
    thread::JoinHandle,
    time::Duration,
};

pub(crate) enum Message {
    SkipToTimestamp(i64),
    UpdateParameters(AudioParameters),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AudioParameters {
    pub sample_rate: u32,
    pub channels: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioMetadata {
    pub index: usize,
    pub time_base: ffn::Rational,
    pub sample_rate: u32,
    pub channels: u16,
    pub(crate) format: ffn::format::Sample,
    pub(crate) channel_layout: ffn::ChannelLayout,
    pub(crate) frame_size: u32,
}

unsafe impl Send for AudioMetadata {}
unsafe impl Sync for AudioMetadata {}

impl Default for AudioMetadata {
    fn default() -> Self {
        AudioMetadata {
            index: usize::MAX,
            time_base: ffn::Rational::new(0, 1),
            sample_rate: 0,
            channels: 0,
            format: ffn::format::Sample::None,
            channel_layout: ffn::ChannelLayout::STEREO,
            frame_size: 0,
        }
    }
}

pub(crate) struct AudioStream {
    pub metadata: AudioMetadata,
    pub messages: Sender<Message>,
    pub packets: PacketSender,
    pub frames: FrameQueue,
}

pub(crate) struct AudioDecoder {
    pub decoder: ffn::decoder::Audio,
    pub metadata: AudioMetadata,
}

impl AudioDecoder {
    pub fn new(input: &mut Input) -> Result<Self> {
        let stream = input
            .format_ctx
            .streams()
            .best(ffn::media::Type::Audio)
            .ok_or(Error::InvalidStream)?;

        let stream_index = stream.index();

        let codec = stream.parameters().id();
        let decoder = ffn::decoder::find(codec).ok_or(Error::MissingCodec(codec.name()))?;

        let mut decoder = ffn::codec::Context::new_with_codec(decoder).decoder();
        decoder.set_parameters(stream.parameters())?;
        decoder.set_threading(ffn::threading::Config {
            kind: ffn::threading::Type::Frame,
            count: 0,
        });

        let decoder = decoder.audio()?;

        let sample_rate = decoder.rate();
        let channels = decoder.channels();
        let format = decoder.format();
        let channel_layout = decoder.channel_layout();
        let frame_size = decoder.frame_size();

        let metadata = AudioMetadata {
            index: stream_index,
            time_base: decoder.time_base(),
            sample_rate,
            channels,
            format,
            channel_layout,
            frame_size,
        };

        Ok(AudioDecoder { decoder, metadata })
    }
}

unsafe impl Send for AudioDecoder {}
unsafe impl Sync for AudioDecoder {}

struct ResamplerState {
    parameters: AudioParameters,
    resampler: ffn::software::resampling::Context,
    frame: ffn::util::frame::Audio,
}

pub(crate) struct AudioThread {
    decoder: AudioDecoder,
    state: Arc<DecoderState>,
    audio_rx: PacketReceiver,
    frame_queue: FrameQueue,
    resampler: Option<ResamplerState>,
    messages: Receiver<Message>,
}

impl AudioThread {
    pub fn new(
        decoder: AudioDecoder,
        pbs: Arc<DecoderState>,
        audio_rx: PacketReceiver,
        frame_queue: FrameQueue,
        messages: Receiver<Message>,
    ) -> Self {
        AudioThread {
            decoder,
            state: pbs,
            audio_rx,
            frame_queue,
            resampler: None,
            messages,
        }
    }

    fn flush(&mut self) {
        self.decoder.decoder.flush();

        if let Some(resampler) = &mut self.resampler {
            while let Ok(Some(_)) = resampler.resampler.flush(&mut resampler.frame) {}
        }
    }

    fn update_parameters(&mut self, parameters: AudioParameters) -> Result<()> {
        if self
            .resampler
            .as_ref()
            .is_none_or(|resampler| resampler.parameters != parameters)
        {
            let stream = self.state.audio_stream.load().clone();

            let format = ffn::format::Sample::F32(ffn::format::sample::Type::Packed);
            let channel_layout = ffn::ChannelLayout(ff::AVChannelLayout {
                order: ff::AVChannelOrder::AV_CHANNEL_ORDER_NATIVE,
                nb_channels: parameters.channels as _,
                u: ff::AVChannelLayout__bindgen_ty_1 {
                    mask: (1 << parameters.channels) - 1,
                },
                opaque: std::ptr::null_mut(),
            });

            let resampler = ffn::software::resampler(
                (
                    stream.metadata.format,
                    stream.metadata.channel_layout,
                    stream.metadata.sample_rate,
                ),
                (format, channel_layout, parameters.sample_rate),
            )?;

            let frame = ffn::util::frame::Audio::new(
                format,
                (stream.metadata.frame_size * parameters.sample_rate / stream.metadata.sample_rate)
                    as _,
                channel_layout,
            );

            self.resampler = Some(ResamplerState {
                parameters,
                resampler,
                frame,
            });
        }
        Ok(())
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
                    Message::UpdateParameters(parameters) => {
                        if let Err(error) = self.update_parameters(parameters) {
                            log::error!("audio thread failed to update parameters: {}", error);
                        }
                    }
                }
            }

            let mut prev_frame = None;

            while self.state.alive.load(Ordering::Relaxed) {
                if packet_serial != self.audio_rx.metadata.serial.load(Ordering::Relaxed) {
                    self.flush();
                }

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
                            self.flush();
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
                    skip_to_ts = None;

                    let mut audio_frame = ManuallyDrop::new(unsafe {
                        ffn::util::frame::Audio::wrap(frame.as_mut_ptr())
                    });

                    // TODO: automatically recreate sampler if input frame format changed
                    if let Some(resampler) = &mut self.resampler {
                        let resampled_pts = unsafe {
                            ff::swr_next_pts(
                                resampler.resampler.as_mut_ptr(),
                                (*frame.as_ptr()).pts * resampler.parameters.sample_rate as i64,
                            )
                        };

                        let resampled_pts =
                            resampled_pts / self.decoder.metadata.sample_rate as i64;

                        if audio_frame.channel_layout().0.order
                            == ff::AVChannelOrder::AV_CHANNEL_ORDER_UNSPEC
                        {
                            let channels = audio_frame.channels();
                            audio_frame
                                .set_channel_layout(ffn::ChannelLayout::default(channels as _));
                        }

                        if let Err(error) =
                            resampler.resampler.run(&audio_frame, &mut resampler.frame)
                        {
                            log::error!("audio resampler failed: {}", error);
                        } else {
                            unsafe {
                                ff::av_frame_unref(audio_frame.as_mut_ptr());
                                audio_frame.alloc(
                                    resampler.frame.format(),
                                    resampler.frame.samples(),
                                    resampler.frame.channel_layout(),
                                );
                                ff::av_frame_copy(
                                    audio_frame.as_mut_ptr(),
                                    resampler.frame.as_mut_ptr(),
                                );
                                ff::av_frame_copy_props(
                                    audio_frame.as_mut_ptr(),
                                    resampler.frame.as_mut_ptr(),
                                );
                                audio_frame.set_pts(Some(resampled_pts));
                            }
                        }
                    }

                    self.frame_queue.send(frame, packet_serial);
                }
            }

            let packet = loop {
                if !self.state.alive.load(Ordering::Relaxed) {
                    break 'exit;
                }

                let Some(packet) = self.audio_rx.receive() else {
                    continue;
                };

                if packet_serial != packet.serial {
                    self.flush();
                    packet_serial = packet.serial;
                }

                if packet_serial == self.audio_rx.metadata.serial.load(Ordering::SeqCst) {
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

pub struct AudioSink {
    state: Arc<DecoderState>,
    frame_queue: FrameQueue,
    messages: Sender<Message>,
    parameters: AudioParameters,
    queue: Arc<PacketQueueMetadata>,
    clock: Arc<Clock>,
    last_pts: f64,
    last_serial: u32,
    samples: Vec<f32>,
}

impl AudioSink {
    pub(crate) fn new(
        pbs: Arc<DecoderState>,
        frame_queue: FrameQueue,
        messages: Sender<Message>,
        queue: Arc<PacketQueueMetadata>,
        clock: Arc<Clock>,
    ) -> Self {
        let mut sink = AudioSink {
            state: pbs,
            frame_queue,
            messages,
            parameters: AudioParameters {
                sample_rate: 0,
                channels: 0,
            },
            queue,
            clock,
            last_pts: 0.,
            last_serial: 0,
            samples: vec![],
        };
        sink.set_parameters(AudioParameters {
            sample_rate: 48000,
            channels: 2,
        });
        sink
    }

    pub fn set_parameters(&mut self, parameters: AudioParameters) {
        if self.parameters != parameters {
            self.parameters = parameters;
            if let Err(_) = self.messages.send(Message::UpdateParameters(parameters)) {
                log::error!("cannot update parameters, audio thread closed");
            }
        }
    }

    #[inline]
    pub fn parameters(&self) -> AudioParameters {
        self.parameters
    }

    pub fn sample_rate(&self) -> u32 {
        self.state.audio_stream.load().metadata.sample_rate
    }

    pub fn channels(&self) -> u16 {
        self.state.audio_stream.load().metadata.channels
    }

    pub fn read_to_slice(&mut self, out: &mut [f32], gain: f32) -> Result<()> {
        let gain = gain.max(0.);

        out.fill(0.);

        let time = ffn::time::relative() as f64 / 1000000.;

        if self.state.play_state() == PlayState::Paused {
            return Ok(());
        }

        while self.samples.len() < out.len() {
            let Some(frame) = self.frame_queue.try_next() else {
                return Ok(());
            };

            let serial = frame.serial;
            if serial != self.queue.serial.load(Ordering::Relaxed) {
                self.frame_queue.release(frame);
                continue;
            }

            let mut frame = ManuallyDrop::new(ffn::util::frame::Audio::from(frame.frame));

            let format = ffn::format::Sample::F32(ffn::format::sample::Type::Packed);

            if frame.rate() != self.parameters.sample_rate
                || frame.format() != format
                || frame.channels() != self.parameters.channels
            {
                log::warn!("audio frame does not match requested audio parameters, discarding");
                unsafe { ff::av_frame_unref(frame.as_mut_ptr()) };
                self.frame_queue.release(Frame {
                    frame: unsafe { ffn::Frame::wrap(frame.as_mut_ptr()) },
                    serial,
                });
                return Ok(());
            }

            self.last_pts = if let Some(pts) = frame.pts() {
                pts as f64 * f64::from(self.state.audio_stream.load().metadata.time_base)
                    + frame.samples() as f64 / frame.rate() as f64
            } else {
                f64::NAN
            };
            self.last_serial = serial;

            let (_, samples, _) = unsafe { frame.data(0).align_to::<f32>() };
            self.samples.extend_from_slice(samples);

            self.frame_queue.release(Frame {
                frame: unsafe { ffn::Frame::wrap(frame.as_mut_ptr()) },
                serial,
            });
        }

        for (i, x) in self.samples.drain(..out.len()).enumerate() {
            out[i] = x * gain;
        }

        if !self.last_pts.is_nan() {
            // last_pts tells us the pts at the very end of self.samples
            // so the current pts (where the audio sink is currently at)
            // is the last_pts - buffered (unread) samples duration
            let target_rate = self.parameters.sample_rate * self.parameters.channels as u32;
            self.clock.set(
                self.last_pts - self.samples.len() as f64 / target_rate as f64,
                self.last_serial,
                Some(time),
            );
        }

        Ok(())
    }

    #[cfg(feature = "cpal")]
    pub fn into_device_sink(mut self) -> DeviceAudioSink {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

        let device = cpal::default_host()
            .default_output_device()
            .expect("default cpal output");
        let config = device
            .default_output_config()
            .expect("default cpal output config");

        self.set_parameters(AudioParameters {
            sample_rate: config.sample_rate(),
            channels: config.channels(),
        });

        let gain = Arc::new(AtomicF32::new(1.));

        let stream = {
            let gain = gain.clone();
            device
                .build_output_stream(
                    &config.config(),
                    move |data: &mut [f32], _| {
                        if let Err(error) = self.read_to_slice(data, gain.load(Ordering::Relaxed)) {
                            log::error!("failed to read audio samples: {}", error);
                        }
                    },
                    |err| {
                        log::error!("cpal device encountered an error: {}", err);
                    },
                    None,
                )
                .expect("cpal output stream")
        };

        stream.play().expect("play cpal stream");

        DeviceAudioSink {
            _stream: stream,
            gain,
        }
    }
}

#[cfg(feature = "cpal")]
pub struct DeviceAudioSink {
    _stream: cpal::Stream,
    gain: Arc<AtomicF32>,
}

impl DeviceAudioSink {
    pub fn set_gain(&self, gain: f32) {
        self.gain.store(gain.max(0.), Ordering::Relaxed);
    }

    pub fn gain(&self) -> f32 {
        self.gain.load(Ordering::Relaxed)
    }
}
