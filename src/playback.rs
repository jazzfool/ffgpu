use crate::decode::Decoder;
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use ffmpeg_sys_next as ff;
use std::{
    ptr::NonNull,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI64, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

// the following playback code is loosely based on ffplay.c

pub(crate) struct PlaybackState {
    alive: AtomicBool,
    paused: AtomicBool,
    is_eof: AtomicBool,
}

impl PlaybackState {
    pub fn new() -> Self {
        PlaybackState {
            alive: AtomicBool::new(true),
            paused: AtomicBool::new(false),
            is_eof: AtomicBool::new(false),
        }
    }

    pub fn paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    pub fn kill(&self) {
        self.alive.store(false, Ordering::SeqCst);
    }
}

pub(crate) struct PacketQueueMetadata {
    duration: AtomicI64,
}

#[derive(Clone)]
pub(crate) struct PacketSender {
    metadata: Arc<PacketQueueMetadata>,
    tx: Sender<NonNull<ff::AVPacket>>,
}

impl PacketSender {
    const MIN_FRAMES: usize = 25;

    fn push(&self, packet: NonNull<ff::AVPacket>) {
        let copy = unsafe { ff::av_packet_alloc() };
        if copy.is_null() {
            unsafe { ff::av_packet_unref(packet.as_ptr()) };
            log::error!("failed to allocate AVPacket");
            return;
        }
        // SAFETY: copy.is_null() handled above
        let copy = unsafe { NonNull::new_unchecked(copy) };
        unsafe { ff::av_packet_move_ref(copy.as_ptr(), packet.as_ptr()) };

        // SAFETY: caller must ensure that *packet is a valid AVPacket
        self.metadata
            .duration
            .fetch_add(unsafe { packet.as_ref().duration }, Ordering::SeqCst);
        self.tx.send(copy).unwrap();
    }

    unsafe fn push_null(&self, mut packet: NonNull<ff::AVPacket>, stream_index: i32) {
        unsafe {
            packet.as_mut().stream_index = stream_index;
        }
        self.push(packet);
    }

    fn has_enough_packets(&self, time_base: ff::AVRational) -> bool {
        self.tx.len() > Self::MIN_FRAMES
            && (unsafe { ff::av_q2d(time_base) }
                * self.metadata.duration.load(Ordering::SeqCst) as f64)
                > 1.0
    }
}

unsafe impl Send for PacketSender {}
unsafe impl Sync for PacketSender {}

pub(crate) struct PacketReceiver {
    metadata: Arc<PacketQueueMetadata>,
    rx: Receiver<NonNull<ff::AVPacket>>,
}

impl PacketReceiver {
    fn try_receive(&self, packet: NonNull<ff::AVPacket>) -> bool {
        let Ok(recv) = self.rx.try_recv() else {
            return false;
        };
        self.metadata
            .duration
            .fetch_sub(unsafe { recv.as_ref().duration }, Ordering::SeqCst);
        unsafe {
            ff::av_packet_move_ref(packet.as_ptr(), recv.as_ptr());
            ff::av_packet_free(&mut recv.as_ptr());
        }
        true
    }
}

unsafe impl Send for PacketReceiver {}
unsafe impl Sync for PacketReceiver {}

pub(crate) fn packet_queue() -> (PacketSender, PacketReceiver) {
    let metadata = Arc::new(PacketQueueMetadata {
        duration: AtomicI64::new(0),
    });
    let (tx, rx) = unbounded();
    let tx = PacketSender {
        metadata: metadata.clone(),
        tx,
    };
    let rx = PacketReceiver { metadata, rx };
    (tx, rx)
}

pub(crate) struct ReadThread {
    decoder: Arc<Decoder>,
    pbs: Arc<PlaybackState>,
    was_paused: bool,
    video_tx: PacketSender,
    frame_queue: FrameQueue,
}

impl ReadThread {
    pub fn new(
        decoder: Arc<Decoder>,
        pbs: Arc<PlaybackState>,
        video_tx: PacketSender,
        frame_queue: FrameQueue,
    ) -> Self {
        ReadThread {
            decoder,
            pbs,
            was_paused: false,
            video_tx,
            frame_queue,
        }
    }

    fn run_thread(&mut self) {
        const MAX_QUEUE_SIZE: usize = 15 * 1024 * 1024;

        let pkt = unsafe { NonNull::new(ff::av_packet_alloc()).expect("av_packet_alloc") };

        let time_base = unsafe { (*self.decoder.video_stream).time_base };

        while self.pbs.alive.load(Ordering::Relaxed) {
            let paused = self.pbs.paused.load(Ordering::Relaxed);
            if self.was_paused != paused {
                self.was_paused = paused;
                if self.was_paused {
                    unsafe { ff::av_read_pause(self.decoder.format_ctx) };
                } else {
                    unsafe { ff::av_read_play(self.decoder.format_ctx) };
                }
            }

            if (self.video_tx.tx.len()/*+ self.audio_tx.packets.len()
            + self.subtitle_tx.packets.len()*/)
                > MAX_QUEUE_SIZE
                || self.video_tx.has_enough_packets(time_base)
            {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }

            let is_eof = self.pbs.is_eof.load(Ordering::SeqCst);

            if !paused && is_eof && self.frame_queue.queue_rx.is_empty() {
                // TODO: loop video if looping is enabled
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }

            let ret = unsafe { ff::av_read_frame(self.decoder.format_ctx, pkt.as_ptr()) };
            if ret < 0 {
                // either an error occurred or EOF

                if !is_eof
                    && (ret == ff::AVERROR_EOF
                        || unsafe { ff::avio_feof((*self.decoder.format_ctx).pb) != 0 })
                {
                    unsafe {
                        // ensure the other threads wake up to handle EOF
                        self.video_tx.push_null(pkt, self.decoder.video_stream_idx);
                        // self.audio_q.push_null();
                        // self.subtitle_q.push_null();
                    }
                    self.pbs.is_eof.store(true, Ordering::SeqCst);
                }

                let avio_error = unsafe { (*(*self.decoder.format_ctx).pb).error };
                if avio_error != 0 {
                    log::error!("AVIOContext error {}", avio_error);
                }

                std::thread::sleep(Duration::from_millis(10));
                continue;
            } else {
                self.pbs.is_eof.store(false, Ordering::SeqCst);
            }

            let stream_index = unsafe { pkt.as_ref().stream_index };
            if stream_index == self.decoder.video_stream_idx {
                self.video_tx.push(pkt);
            } else {
                unsafe { ff::av_packet_unref(pkt.as_ptr()) };
            }
            // TODO: handle audio/subtitle packets
        }

        unsafe { ff::av_packet_free(&mut pkt.as_ptr()) };
    }

    pub fn run(mut self) -> JoinHandle<()> {
        std::thread::spawn(move || {
            self.run_thread();
        })
    }
}

unsafe impl Send for ReadThread {}
unsafe impl Sync for ReadThread {}

struct FramePool {
    frames: Vec<NonNull<ff::AVFrame>>,
}

impl Drop for FramePool {
    fn drop(&mut self) {
        for frame in &self.frames {
            unsafe {
                ff::av_frame_unref(frame.as_ptr());
                ff::av_frame_free(&mut frame.as_ptr());
            }
        }
    }
}

#[derive(Clone)]
pub(crate) struct FrameQueue {
    free_tx: Sender<NonNull<ff::AVFrame>>,
    free_rx: Receiver<NonNull<ff::AVFrame>>,
    queue_tx: Sender<NonNull<ff::AVFrame>>,
    queue_rx: Receiver<NonNull<ff::AVFrame>>,
    _pool: Arc<FramePool>,
}

impl FrameQueue {
    pub fn new(capacity: usize) -> Self {
        let (free_tx, free_rx) = bounded(capacity);
        let (queue_tx, queue_rx) = bounded(capacity);

        let mut pool = FramePool { frames: vec![] };

        for _ in 0..capacity {
            let frame = unsafe { NonNull::new(ff::av_frame_alloc()).expect("av_frame_alloc") };
            pool.frames.push(frame);
            free_tx.send(frame).unwrap();
        }

        let pool = Arc::new(pool);

        FrameQueue {
            free_tx,
            free_rx,
            queue_tx,
            queue_rx,
            _pool: pool,
        }
    }

    pub fn try_acquire(&self) -> Option<NonNull<ff::AVFrame>> {
        self.free_rx.try_recv().ok()
    }

    pub fn send(&self, frame: NonNull<ff::AVFrame>) {
        self.queue_tx.send(frame).unwrap();
    }

    pub fn queued_len(&self) -> usize {
        self.queue_rx.len()
    }

    pub fn try_next(&self) -> Option<NonNull<ff::AVFrame>> {
        self.queue_rx.try_recv().ok()
    }

    pub fn release(&self, frame: NonNull<ff::AVFrame>) {
        unsafe { ff::av_frame_unref(frame.as_ptr()) };
        self.free_tx.send(frame).unwrap();
    }
}

unsafe impl Send for FrameQueue {}
unsafe impl Sync for FrameQueue {}

pub(crate) struct VideoThread {
    decoder: Arc<Decoder>,
    pbs: Arc<PlaybackState>,
    video_rx: PacketReceiver,
    frame_queue: FrameQueue,
}

impl VideoThread {
    pub fn new(
        decoder: Arc<Decoder>,
        pbs: Arc<PlaybackState>,
        video_rx: PacketReceiver,
        frame_queue: FrameQueue,
    ) -> Self {
        VideoThread {
            decoder,
            pbs,
            video_rx,
            frame_queue,
        }
    }

    fn decode_video_frame(&self, frame: NonNull<ff::AVFrame>, pkt: NonNull<ff::AVPacket>) -> bool {
        while self.pbs.alive.load(Ordering::Relaxed) {
            loop {
                let ret =
                    unsafe { ff::avcodec_receive_frame(self.decoder.decoder_ctx, frame.as_ptr()) };
                if ret == ff::AVERROR_EOF {
                    unsafe { ff::avcodec_flush_buffers(self.decoder.decoder_ctx) };
                    return false;
                }
                if ret >= 0 {
                    return true;
                }
                if ret == ff::AVERROR(ff::EAGAIN) {
                    break;
                }
            }

            if !self.video_rx.try_receive(pkt) {
                return false;
            }

            unsafe {
                let ret = ff::avcodec_send_packet(self.decoder.decoder_ctx, pkt.as_ptr());
                if ret != ff::AVERROR(ff::EAGAIN) {
                    ff::av_packet_unref(pkt.as_ptr());
                }
            }
            // TODO: drop early video frames when master clock is audio
        }
        false
    }

    fn run_thread(&mut self) {
        let frame = unsafe { NonNull::new(ff::av_frame_alloc()).expect("av_frame_alloc") };
        let pkt = unsafe { NonNull::new(ff::av_packet_alloc()).expect("av_packet_alloc") };

        let mut queued = None;
        while self.pbs.alive.load(Ordering::Relaxed) {
            let queued_frame = queued.take().or_else(|| self.frame_queue.try_acquire());
            let Some(queued_frame) = queued_frame else {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            };
            if self.decode_video_frame(frame, pkt) {
                unsafe {
                    ff::av_frame_move_ref(queued_frame.as_ptr(), frame.as_ptr());
                    ff::av_frame_unref(frame.as_ptr());
                }
                self.frame_queue.send(queued_frame);
            } else {
                queued = Some(queued_frame);
            }
        }

        unsafe { ff::av_frame_free(&mut frame.as_ptr()) };
        unsafe { ff::av_packet_free(&mut pkt.as_ptr()) };
    }

    pub fn run(mut self) -> JoinHandle<()> {
        std::thread::spawn(move || {
            self.run_thread();
        })
    }
}
