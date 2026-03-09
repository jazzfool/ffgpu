use crate::{decode::Decoder, video::Position};
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use ffmpeg_next::{self as ffn, packet::Ref, sys as ff};
use std::{
    i64,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

// the following playback code is loosely based on ffplay.c

pub(crate) struct PlaybackState {
    pub(crate) alive: AtomicBool,
    pub(crate) paused: AtomicBool,
    pub(crate) is_eof: AtomicBool,
    pub(crate) current_pts: AtomicI64,
}

impl PlaybackState {
    pub fn new() -> Self {
        PlaybackState {
            alive: AtomicBool::new(true),
            paused: AtomicBool::new(false),
            is_eof: AtomicBool::new(false),
            current_pts: AtomicI64::new(0),
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

#[derive(Clone)]
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

#[derive(Debug)]
pub(crate) enum ReadMessage {
    SeekStream(Position),
}

pub(crate) struct ReadThread {
    decoder: Arc<Mutex<Decoder>>,
    pbs: Arc<PlaybackState>,
    was_paused: bool,
    video_tx: PacketSender,
    video_rx: PacketReceiver,
    frame_queue: FrameQueue,
    messages: Receiver<ReadMessage>,
}

impl ReadThread {
    pub fn new(
        decoder: Arc<Mutex<Decoder>>,
        pbs: Arc<PlaybackState>,
        video_tx: PacketSender,
        video_rx: PacketReceiver,
        frame_queue: FrameQueue,
        messages: Receiver<ReadMessage>,
    ) -> Self {
        ReadThread {
            decoder,
            pbs,
            was_paused: false,
            video_tx,
            video_rx,
            frame_queue,
            messages,
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
                            // flush
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

            while let Ok(message) = self.messages.try_recv() {
                match message {
                    ReadMessage::SeekStream(position) => {
                        if let Err(error) = {
                            let (ts, flags) = match position {
                                Position::Time(duration) => {
                                    let ts =
                                        (duration.as_secs_f64() * ff::AV_TIME_BASE as f64) as i64;
                                    (ts, 0)
                                }
                                Position::Frame(frame) => (frame as i64, ff::AVSEEK_FLAG_FRAME),
                            };
                            let err = unsafe {
                                ff::avformat_seek_file(
                                    decoder.format_ctx.as_mut_ptr(),
                                    -1,
                                    i64::MIN,
                                    ts,
                                    i64::MAX,
                                    flags,
                                )
                            };
                            (err == 0).then_some(()).ok_or(ffn::Error::from(err))
                        } {
                            log::error!("failed to seek stream: {}", error);
                        } else {
                            decoder.decoder_ctx.flush();
                            self.pbs.is_eof.store(false, Ordering::SeqCst);
                            while let Some(frame) = self.frame_queue.try_next() {
                                self.frame_queue.release(frame);
                            }
                            while let Some(_) = self.video_rx.try_receive() {
                                /* flush packet queue */
                            }
                            /*unsafe {
                                self.video_tx
                                    .push_null(ffn::Packet::empty(), decoder.video_stream_index);
                            }*/
                        }
                    }
                }
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

    pub fn send(&self, frame: &mut ffn::Frame) {
        if let Ok(mut dst) = self.free_rx.recv() {
            unsafe {
                ff::av_frame_move_ref(dst.as_mut_ptr(), frame.as_mut_ptr());
                ff::av_frame_unref(frame.as_mut_ptr());
            }
            self.queue_tx.send(dst).unwrap();
        }
    }

    pub fn queued_len(&self) -> usize {
        self.queue_rx.len()
    }

    pub fn try_next(&self) -> Option<ffn::Frame> {
        self.queue_rx.try_recv().ok()
    }

    pub fn release(&self, mut frame: ffn::Frame) {
        unsafe { ff::av_frame_unref(frame.as_mut_ptr()) };
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

    fn run_thread(&mut self) {
        let mut frame = unsafe { ffn::Frame::empty() };
        while self.pbs.alive.load(Ordering::Relaxed) {
            let mut decoder = self.decoder.lock().expect("lock decoder");

            let Some(packet) = self.video_rx.try_receive() else {
                continue;
            };

            if let Err(error) = decoder.decoder_ctx.send_packet(&packet) {
                log::error!("failed to send packet: {}", error);
            }

            loop {
                if self.frame_queue.free_rx.is_empty() {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }

                match decoder.decoder_ctx.receive_frame(&mut frame) {
                    Ok(_) => {
                        if let Some(pts) = frame.pts() {
                            self.pbs.current_pts.store(pts, Ordering::SeqCst);
                        }
                        self.frame_queue.send(&mut frame);
                    }
                    Err(ffn::Error::Eof) => {
                        decoder.decoder_ctx.flush();
                        break;
                    }
                    Err(ffn::Error::Other { errno: ff::EAGAIN }) => {
                        break;
                    }
                    _ => {}
                }
            }
        }
    }

    pub fn run(mut self) -> JoinHandle<()> {
        std::thread::spawn(move || {
            self.run_thread();
        })
    }
}
