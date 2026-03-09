use crate::{
    context::pipeline_cache::PipelineCache,
    decode::{Decoder, FrameDecoder, QueryInfo},
    error::Result,
    playback::{
        Frame, FrameQueue, PacketQueueMetadata, PlaybackState, ReadMessage, ReadThread,
        VideoThread, packet_queue,
    },
};
use crossbeam_channel::{Sender, unbounded};
use ffmpeg_next::sys as ff;
use std::{
    path::Path,
    sync::{Arc, atomic::Ordering},
    thread::JoinHandle,
    time::Duration,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Position {
    Time(Duration),
    Frame(u64),
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

    looping: bool,
    frame_timer: f64,
    last_pts: i64,
    last_serial: u32,
    queued_frame: Option<Frame>,
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

        let video_thread = VideoThread::new(
            stream_decoder.clone(),
            pbs.clone(),
            video_rx,
            frame_queue.clone(),
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

            looping: false,
            frame_timer: 0.0,
            last_pts: 0,
            last_serial: 0,
            queued_frame: None,
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
        retry: &mut bool,
        wait_duration: &mut Duration,
    ) -> bool {
        let time = unsafe { ff::av_gettime_relative() as f64 / 1000000.0 };

        if self.frame_queue.queued_len() == 0
            && self.pbs.is_eof.load(Ordering::SeqCst)
            && self.looping
        {
            // eof reached
            self.seek(Position::Frame(0));
        }

        if frame.serial != self.video_queue.serial.load(Ordering::SeqCst) {
            self.frame_timer = time;
            *retry = true;
            return true;
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
            *retry = true;
            return false;
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
            )
        };

        if self.pbs.step.fetch_and(false, Ordering::SeqCst) {
            self.set_paused(true);
        }

        true
    }

    pub fn update(&mut self, encoder: &mut wgpu::CommandEncoder) -> Duration {
        if self.pbs.paused() || self.frame_queue.queued_len() == 0 {
            return Duration::from_millis(5);
        }

        let mut duration = Duration::from_secs_f64(f64::from(self.query_info.framerate.invert()));
        loop {
            let queued_len = self.frame_queue.queued_len();
            let frame = self
                .queued_frame
                .take()
                .or_else(|| self.frame_queue.try_next());
            if let Some(frame) = frame {
                let mut retry = false;
                let pop = self.update_frame(encoder, &frame, queued_len, &mut retry, &mut duration);
                if !pop {
                    self.queued_frame = Some(frame);
                } else {
                    self.frame_queue.release(frame);
                }
                if !retry {
                    break;
                }
            } else {
                break;
            }
        }
        duration
    }

    pub fn position(&self) -> Duration {
        Duration::from_secs_f64(
            self.pbs.current_pts.load(Ordering::SeqCst) as f64
                * f64::from(self.query_info.time_base),
        )
    }

    pub fn seek(&mut self, position: impl Into<Position>) {
        if let Err(error) = self
            .read_messages
            .send(ReadMessage::SeekStream(position.into()))
        {
            log::error!("failed to send seek message: {}", error);
        }
        if let Some(queued_frame) = self.queued_frame.take() {
            self.frame_queue.release(queued_frame);
        }
        // TODO: force video thread to step one frame if paused
    }

    pub fn paused(&self) -> bool {
        self.pbs.paused.load(Ordering::SeqCst)
    }

    pub fn set_paused(&mut self, paused: bool) {
        self.pbs.paused.store(paused, Ordering::SeqCst);
    }

    pub fn looping(&self) -> bool {
        self.looping
    }

    pub fn set_looping(&mut self, looping: bool) {
        self.looping = looping;
    }

    pub fn step_one_frame(&self) {
        self.pbs.paused.store(false, Ordering::SeqCst);
        self.pbs.step.store(true, Ordering::SeqCst);
    }
}

impl Drop for Video {
    fn drop(&mut self) {
        self.pbs.kill();

        if let Some(video_thread) = self.video_thread.take() {
            video_thread.join().unwrap();
        }

        if let Some(read_thread) = self.read_thread.take() {
            read_thread.join().unwrap();
        }
    }
}
