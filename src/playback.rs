use crate::decode::Decoder;
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use ffmpeg_next::{self as ffn, sys as ff};
use std::{
    sync::{
        Arc, Mutex,
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
    tx: Sender<ffn::Packet>,
}

impl PacketSender {
    const MIN_FRAMES: usize = 25;

    fn push(&self, packet: ffn::Packet) {
        self.metadata
            .duration
            .fetch_add(packet.duration(), Ordering::SeqCst);
        self.tx.send(packet).unwrap();
    }

    unsafe fn push_null(&self, mut packet: ffn::Packet, stream_index: usize) {
        packet.set_stream(stream_index);
        self.push(packet);
    }

    fn has_enough_packets(&self, time_base: ffn::Rational) -> bool {
        self.tx.len() > Self::MIN_FRAMES
            && (f64::from(time_base) * self.metadata.duration.load(Ordering::SeqCst) as f64) > 1.0
    }
}

unsafe impl Send for PacketSender {}
unsafe impl Sync for PacketSender {}

pub(crate) struct PacketReceiver {
    metadata: Arc<PacketQueueMetadata>,
    rx: Receiver<ffn::Packet>,
}

impl PacketReceiver {
    fn try_receive(&self) -> Option<ffn::Packet> {
        let Ok(recv) = self.rx.try_recv() else {
            return None;
        };
        self.metadata
            .duration
            .fetch_sub(recv.duration(), Ordering::SeqCst);
        Some(recv)
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
    decoder: Arc<Mutex<Decoder>>,
    pbs: Arc<PlaybackState>,
    was_paused: bool,
    video_tx: PacketSender,
    frame_queue: FrameQueue,
}

impl ReadThread {
    pub fn new(
        decoder: Arc<Mutex<Decoder>>,
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

        let time_base = self
            .decoder
            .lock()
            .expect("lock decoder")
            .query_info
            .time_base;

        while self.pbs.alive.load(Ordering::Relaxed) {
            let paused = self.pbs.paused.load(Ordering::Relaxed);
            if self.was_paused != paused {
                self.was_paused = paused;
                let mut decoder = self.decoder.lock().expect("lock decoder");
                if let Err(error) = if self.was_paused {
                    decoder.format_ctx.pause()
                } else {
                    decoder.format_ctx.play()
                } {
                    log::error!("failed to play/pause stream: {}", error);
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

            let mut packet = ffn::Packet::empty();
            let mut decoder = self.decoder.lock().expect("lock decoder");
            match packet.read(&mut decoder.format_ctx) {
                Ok(_) => {
                    self.pbs.is_eof.store(false, Ordering::SeqCst);
                }
                Err(error) => {
                    if !is_eof
                        && (error == ffn::Error::Eof
                            || unsafe { ff::avio_feof((*decoder.format_ctx.as_ptr()).pb) != 0 })
                    {
                        unsafe {
                            // ensure the other threads wake up to handle EOF
                            self.video_tx
                                .push_null(ffn::Packet::empty(), decoder.video_stream_index);
                            // self.audio_q.push_null();
                            // self.subtitle_q.push_null();
                        }
                        self.pbs.is_eof.store(true, Ordering::SeqCst);
                    }

                    let avio_error = unsafe { (*(*decoder.format_ctx.as_ptr()).pb).error };
                    if avio_error != 0 {
                        log::error!("AVIOContext error {}", avio_error);
                    }

                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
            }

            let stream_index = packet.stream();
            if stream_index == decoder.video_stream_index {
                self.video_tx.push(packet);
            }
            // TODO: handle audio/subtitle packets
        }
    }

    pub fn run(mut self) -> JoinHandle<()> {
        std::thread::spawn(move || {
            self.run_thread();
        })
    }
}

unsafe impl Send for ReadThread {}
unsafe impl Sync for ReadThread {}

#[derive(Clone)]
pub(crate) struct FrameQueue {
    free_tx: Sender<ffn::Frame>,
    free_rx: Receiver<ffn::Frame>,
    queue_tx: Sender<ffn::Frame>,
    queue_rx: Receiver<ffn::Frame>,
}

impl FrameQueue {
    pub fn new(capacity: usize) -> Self {
        let (free_tx, free_rx) = bounded(capacity);
        let (queue_tx, queue_rx) = bounded(capacity);

        for _ in 0..capacity {
            let frame = unsafe { ffn::Frame::empty() };
            free_tx.send(frame).unwrap();
        }

        FrameQueue {
            free_tx,
            free_rx,
            queue_tx,
            queue_rx,
        }
    }

    pub fn try_acquire(&self) -> Option<ffn::Frame> {
        self.free_rx.try_recv().ok()
    }

    pub fn send(&self, frame: ffn::Frame) {
        self.queue_tx.send(frame).unwrap();
    }

    pub fn queued_len(&self) -> usize {
        self.queue_rx.len()
    }

    pub fn try_next(&self) -> Option<ffn::Frame> {
        self.queue_rx.try_recv().ok()
    }

    pub fn release(&self, frame: ffn::Frame) {
        self.free_tx.send(frame).unwrap();
    }
}

unsafe impl Send for FrameQueue {}
unsafe impl Sync for FrameQueue {}

pub(crate) struct VideoThread {
    decoder: Arc<Mutex<Decoder>>,
    pbs: Arc<PlaybackState>,
    video_rx: PacketReceiver,
    frame_queue: FrameQueue,
}

impl VideoThread {
    pub fn new(
        decoder: Arc<Mutex<Decoder>>,
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

    fn decode_video_frame(&self, frame: &mut ffn::Frame) -> bool {
        while self.pbs.alive.load(Ordering::Relaxed) {
            let mut decoder = self.decoder.lock().expect("lock decoder");

            loop {
                match decoder.decoder_ctx.receive_frame(frame) {
                    Ok(_) => return true,
                    Err(ffn::Error::Eof) => {
                        decoder.decoder_ctx.flush();
                        return false;
                    }
                    Err(ffn::Error::Other { errno: ff::EAGAIN }) => {
                        break;
                    }
                    Err(error) => {
                        log::error!("failed to receive frame: {}", error);
                        return false;
                    }
                }
            }

            let Some(packet) = self.video_rx.try_receive() else {
                return false;
            };

            if let Err(error) = decoder.decoder_ctx.send_packet(&packet) {
                log::error!("failed to send packet: {}", error);
            }
            // TODO: drop early video frames when master clock is audio
        }
        false
    }

    fn run_thread(&mut self) {
        let mut queued = None;
        while self.pbs.alive.load(Ordering::Relaxed) {
            let queued_frame = queued.take().or_else(|| self.frame_queue.try_acquire());
            let Some(mut queued_frame) = queued_frame else {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            };
            if self.decode_video_frame(&mut queued_frame) {
                self.frame_queue.send(queued_frame);
            } else {
                queued = Some(queued_frame);
            }
        }
    }

    pub fn run(mut self) -> JoinHandle<()> {
        std::thread::spawn(move || {
            self.run_thread();
        })
    }
}
