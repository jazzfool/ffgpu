use crate::{
    context::pipeline_cache::PipelineCache,
    decode::{
        Clock, DecoderState, Frame, FrameQueue, PlayState,
        audio::{self, AudioDecoder, AudioSink, AudioThread},
        hw::HardwareDecoder,
        packet_queue,
        read::{Input, ReadMessage, ReadThread},
        video,
    },
    error::Result,
};
use crossbeam_channel::{Sender, unbounded};
use ffmpeg_next::{self as ffn, sys as ff};
use std::{
    ops::Add,
    path::Path,
    sync::{Arc, atomic::Ordering},
    thread::JoinHandle,
    time::Duration,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SeekMode {
    Fast,
    Accurate,
}

enum FrameResponse {
    Continue,
    Retry,
    Requeue,
}

pub struct Statistics {
    pub video_clock: f64,
    pub audio_clock: f64,
    pub sync_latency: f64,
    pub decoder_name: &'static str,
}

pub struct Video {
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,

    state: Arc<DecoderState>,
    frame_decoder: video::FrameDecoder,
    read_thread: Option<JoinHandle<()>>,
    video_thread: Option<JoinHandle<()>>,
    audio_thread: Option<JoinHandle<()>>,
    video_clock: Arc<Clock>,
    audio_clock: Arc<Clock>,

    read_messages: Sender<ReadMessage>,

    looping: bool,
    frame_timer: f64,
    last_pts: i64,
    last_serial: u32,
    queued_frame: Option<Frame>,
    step_needs_copy: u8,
}

impl Video {
    pub(crate) fn new<P>(
        instance: wgpu::Instance,
        adapter: wgpu::Adapter,
        device: wgpu::Device,
        queue: wgpu::Queue,
        pipeline_cache: &mut PipelineCache,
        path: &P,
    ) -> Result<(Self, AudioSink)>
    where
        P: AsRef<Path> + ?Sized,
    {
        let mut input = Input::open(path)?;

        let (video_decoder, frame_decoder) =
            video::Decoder::new(&mut input, &device, pipeline_cache)?;

        let audio_decoder = AudioDecoder::new(&mut input)?;

        let (video_tx, video_rx, video_queue) = packet_queue();
        let (audio_tx, audio_rx, audio_queue) = packet_queue();
        let video_frame_queue = FrameQueue::new(18);
        let audio_frame_queue = FrameQueue::new(16);

        let video_clock = Arc::new(Clock::new(video_queue.clone()));
        let audio_clock = Arc::new(Clock::new(audio_queue.clone()));

        let (read_msg_tx, read_msg_rx) = unbounded();
        let (video_msg_tx, video_msg_rx) = unbounded();
        let (audio_msg_tx, audio_msg_rx) = unbounded();

        let video_stream = video::VideoStream {
            metadata: video_decoder.metadata,
            messages: video_msg_tx,
            packets: video_tx,
            frames: video_frame_queue.clone(),
        };

        let audio_stream = audio::AudioStream {
            metadata: audio_decoder.metadata,
            messages: audio_msg_tx.clone(),
            packets: audio_tx,
            frames: audio_frame_queue.clone(),
        };

        let state = Arc::new(DecoderState::new(video_stream, audio_stream));

        let read_thread = ReadThread::new(input, state.clone(), read_msg_rx).run();

        let video_thread = video::VideoThread::new(
            video_decoder,
            state.clone(),
            video_rx,
            video_frame_queue,
            video_msg_rx,
            video_clock.clone(),
            audio_clock.clone(),
        )
        .run();

        let audio_thread = AudioThread::new(
            audio_decoder,
            state.clone(),
            audio_rx,
            audio_frame_queue.clone(),
            audio_msg_rx,
        )
        .run();

        let audio_sink = AudioSink::new(
            state.clone(),
            audio_frame_queue.clone(),
            audio_msg_tx.clone(),
            audio_queue.clone(),
            audio_clock.clone(),
        );

        Ok((
            Video {
                instance,
                adapter,
                device,
                queue,

                state,
                frame_decoder,
                read_thread: Some(read_thread),
                video_thread: Some(video_thread),
                audio_thread: Some(audio_thread),
                video_clock,
                audio_clock,

                read_messages: read_msg_tx,

                looping: false,
                frame_timer: 0.0,
                last_pts: 0,
                last_serial: 0,
                queued_frame: None,
                step_needs_copy: 0,
            },
            audio_sink,
        ))
    }

    pub fn texture(&self) -> &wgpu::Texture {
        self.frame_decoder.texture()
    }

    fn update_frame(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        frame: &Frame,
        queued_len: usize,
        wait_duration: &mut Duration,
    ) -> Result<FrameResponse> {
        let time = ffn::time::relative() as f64 / 1000000.0;

        if frame.serial
            != self
                .state
                .video_stream
                .load()
                .packets
                .metadata
                .serial
                .load(Ordering::SeqCst)
        {
            self.frame_timer = time;
            return Ok(FrameResponse::Retry);
        }

        let best_effort_timestamp = unsafe { (*frame.frame.as_ptr()).best_effort_timestamp };

        let video_info = self.video_info();

        let duration = if frame.serial == self.last_serial {
            (best_effort_timestamp as f64 * f64::from(video_info.time_base))
                - (self.last_pts as f64 * f64::from(video_info.time_base))
        } else {
            0.0
        };
        let duration = if duration < 0.0 || duration > 3600.0 {
            if video_info.framerate.0 > 0 && video_info.framerate.1 > 0 {
                f64::from(video_info.framerate.invert())
            } else {
                0.0
            }
        } else {
            duration
        };

        let mut delay = duration;
        if let Some(video_clock) = self.video_clock.get()
            && let Some(audio_clock) = self.audio_clock.get()
        {
            let diff = video_clock - audio_clock;
            let sync_threshold = delay.clamp(Clock::SYNC_MIN, Clock::SYNC_MAX);
            if diff < -sync_threshold {
                delay = (delay + diff).max(0.);
            } else if diff > sync_threshold && delay > Clock::FRAME_DUPLICATION_THRESHOLD {
                delay += diff;
            } else if diff > sync_threshold {
                delay *= 2.;
            }
        }

        if time < self.frame_timer + delay {
            // too early
            *wait_duration =
                Duration::from_secs_f64(self.frame_timer + delay - time).min(*wait_duration);
            return Ok(FrameResponse::Requeue);
        }

        self.frame_timer += delay;
        if delay > 0.0 && time - self.frame_timer > Clock::SYNC_MAX {
            self.frame_timer = time;
        }

        let pts_sec = best_effort_timestamp as f64
            * f64::from(self.state.video_stream.load().metadata.time_base);
        self.video_clock.set(pts_sec, frame.serial, None);

        if self.state.play_state() != PlayState::Step
            && queued_len > 0
            && time > self.frame_timer + duration
        {
            // drop late frame
            return Ok(FrameResponse::Retry);
        }

        unsafe {
            self.frame_decoder.decode_native_frame(
                &self.instance,
                &self.adapter,
                &self.device,
                &self.queue,
                encoder,
                &frame.frame,
            )?
        };

        if self.state.play_state() == PlayState::Step {
            self.step_needs_copy = self.step_needs_copy.add(4).min(16);
            self.set_paused(true);
        }

        Ok(FrameResponse::Continue)
    }

    fn flush(&mut self) {
        if let Some(queued_frame) = self.queued_frame.take() {
            self.state.video_stream.load().frames.release(queued_frame);
        }

        self.state.video_stream.load().frames.flush();
        self.state.audio_stream.load().frames.flush();

        // TODO: flush samples in AudioSink
    }

    pub fn update(&mut self, encoder: &mut wgpu::CommandEncoder) -> Result<Duration> {
        let play_state = self.state.play_state();

        if play_state == PlayState::Playing {
            self.step_needs_copy = 0;
        }

        if self.step_needs_copy > 0 {
            // NOTE: this is a bit of a hack;
            // on certain backends (namely D3D11VA) we have no way of syncing the D3D11 frame copy with the wgpu YUV->RGB copy.
            // (well, of course it is possible, but wgpu doesn't enable the necessary device extensions...)
            // to counteract this, when stepping a frame (e.g., during accurate seek while paused), we perform the YUV->RGB copy a few more times after the seek.
            self.step_needs_copy -= 1;
            self.frame_decoder.copy_to_rgb(encoder);
        }

        let video_frame_queue = self.state.video_stream.load().frames.clone();

        if video_frame_queue.queued_len() == 0
            && self.state.is_eof.load(Ordering::SeqCst)
            && self.looping
            && play_state != PlayState::Paused
        {
            // eof reached
            self.seek(Duration::ZERO, SeekMode::Fast);
        }

        if play_state == PlayState::Paused || video_frame_queue.queued_len() == 0 {
            return Ok(Duration::from_millis(50));
        }

        let video_info = self.video_info();

        let mut duration = Duration::from_secs_f64(f64::from(video_info.framerate.invert()));
        loop {
            let queued_len = video_frame_queue.queued_len();
            let frame = self
                .queued_frame
                .take()
                .or_else(|| video_frame_queue.try_next());
            if let Some(frame) = frame {
                let response = self.update_frame(encoder, &frame, queued_len, &mut duration)?;
                match response {
                    FrameResponse::Continue => {
                        self.last_pts = unsafe { (*frame.frame.as_ptr()).best_effort_timestamp };
                        self.last_serial = frame.serial;
                        video_frame_queue.release(frame);
                        break;
                    }
                    FrameResponse::Retry => {
                        self.last_pts = unsafe { (*frame.frame.as_ptr()).best_effort_timestamp };
                        self.last_serial = frame.serial;
                        video_frame_queue.release(frame);
                    }
                    FrameResponse::Requeue => {
                        self.queued_frame = Some(frame);
                        break;
                    }
                }
            } else {
                break;
            }
        }

        Ok(duration)
    }

    fn video_info(&self) -> video::VideoMetadata {
        self.state.video_stream.load().metadata
    }

    pub fn statistics(&self) -> Statistics {
        let video_clock = self.video_clock.get().unwrap_or(0.);
        let audio_clock = self.audio_clock.get().unwrap_or(0.);
        let sync_latency = video_clock - audio_clock;
        let decoder_name = self.frame_decoder.hwdec.name();

        Statistics {
            video_clock,
            audio_clock,
            sync_latency,
            decoder_name,
        }
    }

    #[inline]
    pub fn width(&self) -> u32 {
        self.video_info().width
    }

    #[inline]
    pub fn height(&self) -> u32 {
        self.video_info().height
    }

    #[inline]
    pub fn duration(&self) -> Duration {
        self.video_info().duration
    }

    #[inline]
    pub fn framerate(&self) -> f64 {
        let video_info = self.video_info();
        video_info.framerate.0 as f64 / video_info.framerate.1 as f64
    }

    #[inline]
    pub fn decoder_name(&self) -> &'static str {
        self.frame_decoder.hwdec.name()
    }

    pub fn position(&self) -> Duration {
        Duration::from_secs_f64(self.last_pts as f64 * f64::from(self.video_info().time_base))
    }

    pub fn seek(&mut self, position: Duration, mode: SeekMode) {
        let position = position.min(self.duration());
        let ts = (position.as_secs_f64() * ff::AV_TIME_BASE as f64) as i64;

        if let Err(error) = self
            .read_messages
            .send(ReadMessage::SeekStream { ts, mode })
        {
            log::error!("failed to send seek message: {}", error);
        }

        self.flush();
    }

    pub fn paused(&self) -> bool {
        self.state.play_state() != PlayState::Playing
    }

    pub fn set_paused(&mut self, paused: bool) {
        if !paused {
            self.frame_timer += ffn::time::relative() as f64 / 1000000.0
                - self.video_clock.last_updated.load(Ordering::Relaxed);
            self.video_clock.set(
                self.video_clock.get().unwrap_or(0.),
                self.video_clock.serial.load(Ordering::Relaxed),
                None,
            );
        }

        self.video_clock.paused.store(paused, Ordering::Relaxed);
        self.audio_clock.paused.store(paused, Ordering::Relaxed);

        self.state.play_state.store(
            if paused {
                PlayState::Paused
            } else {
                PlayState::Playing
            } as _,
            Ordering::Relaxed,
        );
    }

    pub fn looping(&self) -> bool {
        self.looping
    }

    pub fn set_looping(&mut self, looping: bool) {
        self.looping = looping;
    }

    pub fn step_one_frame(&mut self) {
        self.set_paused(false);
        self.state
            .play_state
            .store(PlayState::Step as _, Ordering::Relaxed);
    }
}

impl Drop for Video {
    fn drop(&mut self) {
        self.flush();

        self.state.kill();

        if let Some(audio_thread) = self.audio_thread.take() {
            audio_thread.join().unwrap();
        }

        if let Some(video_thread) = self.video_thread.take() {
            video_thread.join().unwrap();
        }

        if let Some(read_thread) = self.read_thread.take() {
            read_thread.join().unwrap();
        }
    }
}
