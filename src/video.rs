use crate::{
    context::pipeline_cache::PipelineCache,
    decode::{Decoder, FrameDecoder, QueryInfo},
    error::Result,
    playback::{
        DecodeMessage, Frame, FrameQueue, PacketQueueMetadata, PlayState, PlaybackState,
        ReadMessage, ReadThread, VideoThread, packet_queue,
    },
};
use crossbeam_channel::{Sender, unbounded};
use ffmpeg_next::sys as ff;
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

pub struct Video {
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,

    pbs: Arc<PlaybackState>,
    video_queue: Arc<PacketQueueMetadata>,
    frame_decoder: FrameDecoder,
    frame_queue: FrameQueue,
    read_thread: Option<JoinHandle<()>>,
    video_thread: Option<JoinHandle<()>>,
    query_info: QueryInfo,

    read_messages: Sender<ReadMessage>,
    video_decode_messages: Sender<DecodeMessage>,

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
    ) -> Result<Self>
    where
        P: AsRef<Path> + ?Sized,
    {
        let (stream_decoder, frame_decoder) = Decoder::new(&device, pipeline_cache, path)?;
        let query_info = stream_decoder.query_info;

        let stream_decoder = Arc::new(stream_decoder);

        let pbs = Arc::new(PlaybackState::new());

        let (video_tx, video_rx, video_queue) = packet_queue();
        let frame_queue = FrameQueue::new(18);

        let (read_msg_tx, read_msg_rx) = unbounded();

        let read_thread = ReadThread::new(
            stream_decoder.clone(),
            pbs.clone(),
            video_tx,
            frame_queue.clone(),
            read_msg_rx,
        )
        .run();

        let (video_decode_tx, video_decode_rx) = unbounded();

        let video_thread = VideoThread::new(
            stream_decoder.clone(),
            pbs.clone(),
            video_rx,
            frame_queue.clone(),
            video_decode_rx,
        )
        .run();

        Ok(Video {
            instance,
            adapter,
            device,
            queue,

            pbs,
            video_queue,
            frame_decoder,
            frame_queue,
            read_thread: Some(read_thread),
            video_thread: Some(video_thread),
            query_info,

            read_messages: read_msg_tx,
            video_decode_messages: video_decode_tx,

            looping: false,
            frame_timer: 0.0,
            last_pts: 0,
            last_serial: 0,
            queued_frame: None,
            step_needs_copy: 0,
        })
    }

    pub fn texture(&self) -> &wgpu::Texture {
        self.frame_decoder.texture()
    }

    fn update_frame(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        frame: &Frame,
        _queued_len: usize,
        wait_duration: &mut Duration,
    ) -> Result<FrameResponse> {
        let time = unsafe { ff::av_gettime_relative() as f64 / 1000000.0 };

        if frame.serial != self.video_queue.serial.load(Ordering::SeqCst) {
            self.frame_timer = time;
            return Ok(FrameResponse::Retry);
        }

        let best_effort_timestamp = unsafe { (*frame.frame.as_ptr()).best_effort_timestamp };

        let duration = if frame.serial == self.last_serial {
            (best_effort_timestamp as f64 * f64::from(self.query_info.time_base))
                - (self.last_pts as f64 * f64::from(self.query_info.time_base))
        } else {
            0.0
        };
        let duration = if duration < 0.0 || duration > 3600.0 {
            if self.query_info.framerate.0 > 0 && self.query_info.framerate.1 > 0 {
                f64::from(self.query_info.framerate.invert())
            } else {
                0.0
            }
        } else {
            duration
        };

        let delay = duration; // TODO: we need to add A/V sync latency here

        *wait_duration = Duration::from_secs_f64((self.frame_timer + delay - time).max(0.0));

        if time < self.frame_timer + delay {
            // too early
            return Ok(FrameResponse::Requeue);
        }

        self.frame_timer += delay;
        if delay > 0.0 && time - self.frame_timer > 0.1 {
            self.frame_timer = time;
        }

        *wait_duration = Duration::from_secs_f64((self.frame_timer + delay - time).max(0.0));

        /*if queued_len > 0 {
            *retry = true;
            return true;
        }*/

        self.last_pts = best_effort_timestamp;
        self.last_serial = frame.serial;
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

        if self.pbs.play_state() == PlayState::Step {
            self.step_needs_copy = self.step_needs_copy.add(4).min(16);
            self.pbs
                .play_state
                .store(PlayState::Paused as _, Ordering::Relaxed);
        }

        Ok(FrameResponse::Continue)
    }

    fn flush_frames(&mut self) {
        if let Some(queued_frame) = self.queued_frame.take() {
            self.frame_queue.release(queued_frame);
        }

        while let Some(frame) = self.frame_queue.try_next() {
            self.frame_queue.release(frame);
        }
    }

    pub fn update(&mut self, encoder: &mut wgpu::CommandEncoder) -> Result<Duration> {
        let play_state = self.pbs.play_state();

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

        if self.frame_queue.queued_len() == 0
            && self.pbs.is_eof.load(Ordering::SeqCst)
            && self.looping
            && play_state != PlayState::Paused
        {
            // eof reached
            self.seek(Duration::ZERO, SeekMode::Fast);
        }

        if play_state == PlayState::Paused || self.frame_queue.queued_len() == 0 {
            return Ok(Duration::from_millis(50));
        }

        let mut duration = Duration::from_secs_f64(f64::from(self.query_info.framerate.invert()));
        loop {
            let queued_len = self.frame_queue.queued_len();
            let frame = self
                .queued_frame
                .take()
                .or_else(|| self.frame_queue.try_next());
            if let Some(frame) = frame {
                let response = self.update_frame(encoder, &frame, queued_len, &mut duration)?;
                match response {
                    FrameResponse::Continue => {
                        self.frame_queue.release(frame);
                        break;
                    }
                    FrameResponse::Retry => {
                        self.frame_queue.release(frame);
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

    #[inline]
    pub fn width(&self) -> u32 {
        self.query_info.width
    }

    #[inline]
    pub fn height(&self) -> u32 {
        self.query_info.height
    }

    #[inline]
    pub fn duration(&self) -> Duration {
        self.query_info.duration
    }

    #[inline]
    pub fn framerate(&self) -> f64 {
        self.query_info.framerate.0 as f64 / self.query_info.framerate.1 as f64
    }

    pub fn position(&self) -> Duration {
        Duration::from_secs_f64(self.last_pts as f64 * f64::from(self.query_info.time_base))
    }

    pub fn seek(&mut self, position: Duration, mode: SeekMode) {
        let position = position.min(self.duration());
        let ts = (position.as_secs_f64() * ff::AV_TIME_BASE as f64) as i64;

        if let Err(error) = self.read_messages.send(ReadMessage::SeekStream(ts)) {
            log::error!("failed to send seek message: {}", error);
        }

        self.flush_frames();

        match mode {
            SeekMode::Accurate => {
                let _ = self
                    .video_decode_messages
                    .send(DecodeMessage::SkipToTimestamp(ts));
            }
            _ => {}
        }
    }

    pub fn paused(&self) -> bool {
        self.pbs.play_state() != PlayState::Playing
    }

    pub fn set_paused(&mut self, paused: bool) {
        self.pbs.play_state.store(
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

    pub fn step_one_frame(&self) {
        self.pbs
            .play_state
            .store(PlayState::Step as _, Ordering::Relaxed);
    }
}

impl Drop for Video {
    fn drop(&mut self) {
        self.flush_frames();

        self.pbs.kill();

        if let Some(video_thread) = self.video_thread.take() {
            video_thread.join().unwrap();
        }

        if let Some(read_thread) = self.read_thread.take() {
            read_thread.join().unwrap();
        }
    }
}
