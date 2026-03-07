use crate::{
    context::pipeline_cache::PipelineCache,
    decode::{Decoder, FrameDecoder, QueryInfo},
    error::Result,
    playback::{FrameQueue, PlaybackState, ReadThread, VideoThread, packet_queue},
};
use ffmpeg_next::{self as ffn, sys as ff};
use std::{
    path::Path,
    sync::{Arc, Mutex},
    thread::JoinHandle,
    time::Duration,
};

pub struct Video {
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,

    pbs: Arc<PlaybackState>,
    frame_decoder: FrameDecoder,
    frame_queue: FrameQueue,
    read_thread: Option<JoinHandle<()>>,
    video_thread: Option<JoinHandle<()>>,

    query_info: QueryInfo,
    frame_timer: f64,
    last_pts: i64,
    queued_frame: Option<ffn::Frame>,
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

        let stream_decoder = Arc::new(Mutex::new(stream_decoder));

        let pbs = Arc::new(PlaybackState::new());

        let (video_tx, video_rx) = packet_queue();
        let frame_queue = FrameQueue::new(18);

        let read_thread = ReadThread::new(
            stream_decoder.clone(),
            pbs.clone(),
            video_tx,
            frame_queue.clone(),
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
            frame_decoder,
            frame_queue,
            read_thread: Some(read_thread),
            video_thread: Some(video_thread),

            query_info,
            frame_timer: 0.0,
            last_pts: 0,
            queued_frame: None,
        })
    }

    pub fn texture(&self) -> &wgpu::Texture {
        self.frame_decoder.texture()
    }

    fn update_frame(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        frame: &ffn::Frame,
        _queued_len: usize,
        retry: &mut bool,
        wait_duration: &mut Duration,
    ) -> bool {
        let time = unsafe { ff::av_gettime_relative() as f64 / 1000000.0 };

        let best_effort_timestamp = unsafe { (*frame.as_ptr()).best_effort_timestamp };

        let duration = (best_effort_timestamp as f64 * f64::from(self.query_info.time_base))
            - (self.last_pts as f64 * f64::from(self.query_info.time_base));
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
        unsafe {
            self.frame_decoder.decode_native_frame(
                &self.instance,
                &self.adapter,
                &self.device,
                &self.queue,
                encoder,
                frame,
            )
        };

        true
    }

    pub fn update(&mut self, encoder: &mut wgpu::CommandEncoder) -> Duration {
        if self.pbs.paused() || self.frame_queue.queued_len() == 0 {
            return Duration::from_millis(32);
        }

        // TODO: we should maintain our own frame queue here
        // since we can't peek the actual frame queue
        // if we receive a frame and decide we don't want to show it yet (e.g., based on pts)
        // then we can re-queue it for later

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
