use crate::{decode::Decoder, video::Position};
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use ffmpeg_next::{self as ffn, sys as ff};
use std::{
    i64,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering},
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
    pub(crate) step: AtomicBool,
}

impl PlaybackState {
    pub fn new() -> Self {
        PlaybackState {
            alive: AtomicBool::new(true),
            paused: AtomicBool::new(false),
            is_eof: AtomicBool::new(false),
            current_pts: AtomicI64::new(0),
            step: AtomicBool::new(false),
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
    pub duration: AtomicI64,
    pub serial: AtomicU32,
}

struct Packet {
    pub packet: ffn::Packet,
    pub serial: u32,
}

#[derive(Clone)]
pub(crate) struct PacketSender {
    metadata: Arc<PacketQueueMetadata>,
    rx: Receiver<Packet>,
    tx: Sender<Packet>,
}

impl PacketSender {
    const MIN_FRAMES: usize = 25;

    fn push(&self, packet: ffn::Packet) {
        self.metadata
            .duration
            .fetch_add(packet.duration(), Ordering::SeqCst);
        let serial = self.metadata.serial.load(Ordering::SeqCst);
        self.tx.send(Packet { packet, serial }).unwrap();
    }

    fn push_null(&self, mut packet: ffn::Packet, stream_index: usize) {
        packet.set_stream(stream_index);
        self.push(packet);
    }

    fn has_enough_packets(&self, time_base: ffn::Rational) -> bool {
        self.tx.len() > Self::MIN_FRAMES
            && (f64::from(time_base) * self.metadata.duration.load(Ordering::SeqCst) as f64) > 1.0
    }

    fn flush(&self) {
        while let Ok(_) = self.rx.try_recv() {}
        self.metadata.duration.store(0, Ordering::SeqCst);
        self.metadata.serial.fetch_add(1, Ordering::SeqCst);
    }
}

unsafe impl Send for PacketSender {}
unsafe impl Sync for PacketSender {}

#[derive(Clone)]
pub(crate) struct PacketReceiver {
    metadata: Arc<PacketQueueMetadata>,
    rx: Receiver<Packet>,
}

impl PacketReceiver {
    fn try_receive(&self) -> Option<Packet> {
        let Ok(recv) = self.rx.try_recv() else {
            return None;
        };
        self.metadata
            .duration
            .fetch_sub(recv.packet.duration(), Ordering::SeqCst);
        Some(recv)
    }
}

unsafe impl Send for PacketReceiver {}
unsafe impl Sync for PacketReceiver {}

pub(crate) fn packet_queue() -> (PacketSender, PacketReceiver, Arc<PacketQueueMetadata>) {
    let metadata = Arc::new(PacketQueueMetadata {
        duration: AtomicI64::new(0),
        serial: AtomicU32::new(0),
    });
    let (tx, rx) = unbounded();
    let tx = PacketSender {
        metadata: metadata.clone(),
        rx: rx.clone(),
        tx,
    };
    let rx = PacketReceiver {
        metadata: metadata.clone(),
        rx,
    };
    (tx, rx, metadata)
}

#[derive(Debug)]
pub(crate) enum ReadMessage {
    SeekStream(Position),
}

pub(crate) struct ReadThread {
    decoder: Arc<Decoder>,
    pbs: Arc<PlaybackState>,
    was_paused: bool,
    video_tx: PacketSender,
    frame_queue: FrameQueue,
    messages: Receiver<ReadMessage>,
}

impl ReadThread {
    pub fn new(
        decoder: Arc<Decoder>,
        pbs: Arc<PlaybackState>,
        video_tx: PacketSender,
        frame_queue: FrameQueue,
        messages: Receiver<ReadMessage>,
    ) -> Self {
        ReadThread {
            decoder,
            pbs,
            was_paused: false,
            video_tx,
            frame_queue,
            messages,
        }
    }

    fn run_thread(&mut self) {
        const MAX_QUEUE_SIZE: usize = 15 * 1024 * 1024;

        let time_base = self.decoder.query_info.time_base;

        let mut format_ctx = self
            .decoder
            .format_ctx
            .lock()
            .expect("lock AVFormatContext");

        while self.pbs.alive.load(Ordering::Relaxed) {
            let paused = self.pbs.paused.load(Ordering::Relaxed);
            if self.was_paused != paused {
                self.was_paused = paused;
                if let Err(error) = if self.was_paused {
                    format_ctx.pause()
                } else {
                    format_ctx.play()
                } {
                    log::error!("failed to play/pause stream: {}", error);
                }
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
                                    format_ctx.as_mut_ptr(),
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
                            self.pbs.is_eof.store(false, Ordering::SeqCst);
                            self.video_tx.flush();
                            self.video_tx
                                .push_null(ffn::Packet::empty(), self.decoder.video_stream_index);

                            if paused {
                                self.pbs.step.store(true, Ordering::SeqCst);
                                self.pbs.paused.store(false, Ordering::SeqCst);
                            }
                        }
                    }
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
            match packet.read(&mut format_ctx) {
                Ok(_) => {
                    self.pbs.is_eof.store(false, Ordering::SeqCst);
                }
                Err(error) => {
                    if !is_eof
                        && (error == ffn::Error::Eof
                            || unsafe { ff::avio_feof((*format_ctx.as_ptr()).pb) != 0 })
                    {
                        // flush
                        self.video_tx
                            .push_null(ffn::Packet::empty(), self.decoder.video_stream_index);
                        // self.audio_q.push_null();
                        // self.subtitle_q.push_null();

                        self.pbs.is_eof.store(true, Ordering::SeqCst);
                    }

                    let avio_error = unsafe { (*(*format_ctx.as_ptr()).pb).error };
                    if avio_error != 0 {
                        log::error!("AVIOContext error {}", avio_error);
                    }

                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
            }

            let stream_index = packet.stream();
            if stream_index == self.decoder.video_stream_index {
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

pub(crate) struct Frame {
    pub frame: ffn::Frame,
    pub serial: u32,
}

#[derive(Clone)]
pub(crate) struct FrameQueue {
    free_tx: Sender<Frame>,
    free_rx: Receiver<Frame>,
    queue_tx: Sender<Frame>,
    queue_rx: Receiver<Frame>,
}

impl FrameQueue {
    pub fn new(capacity: usize) -> Self {
        let (free_tx, free_rx) = bounded(capacity);
        let (queue_tx, queue_rx) = bounded(capacity);

        for _ in 0..capacity {
            let frame = unsafe { ffn::Frame::empty() };
            free_tx
                .send(Frame {
                    frame,
                    serial: u32::MAX,
                })
                .unwrap();
        }

        FrameQueue {
            free_tx,
            free_rx,
            queue_tx,
            queue_rx,
        }
    }

    pub fn send(&self, frame: &mut ffn::Frame, serial: u32) {
        if let Ok(mut dst) = self.free_rx.recv() {
            unsafe {
                ff::av_frame_move_ref(dst.frame.as_mut_ptr(), frame.as_mut_ptr());
                ff::av_frame_unref(frame.as_mut_ptr());
            }
            dst.serial = serial;
            self.queue_tx.send(dst).unwrap();
        }
    }

    pub fn queued_len(&self) -> usize {
        self.queue_rx.len()
    }

    pub fn try_next(&self) -> Option<Frame> {
        self.queue_rx.try_recv().ok()
    }

    pub fn release(&self, mut frame: Frame) {
        unsafe { ff::av_frame_unref(frame.frame.as_mut_ptr()) };
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

    fn run_thread(&mut self) {
        let mut packet_serial = 0;

        let mut frame = unsafe { ffn::Frame::empty() };

        let mut decoder_ctx = self
            .decoder
            .decoder_ctx
            .lock()
            .expect("lock AVCodecContext");

        while self.pbs.alive.load(Ordering::Relaxed) {
            if packet_serial == self.video_rx.metadata.serial.load(Ordering::SeqCst) {
                loop {
                    if self.frame_queue.free_rx.is_empty() {
                        std::thread::sleep(Duration::from_millis(10));
                        continue;
                    }

                    match decoder_ctx.receive_frame(&mut frame) {
                        Ok(_) => {
                            if let Some(pts) = frame.pts() {
                                self.pbs.current_pts.store(pts, Ordering::SeqCst);
                            }
                            self.frame_queue.send(&mut frame, packet_serial);
                        }
                        Err(ffn::Error::Eof) => {
                            decoder_ctx.flush();
                            break;
                        }
                        Err(ffn::Error::Other { errno: ff::EAGAIN }) => {
                            break;
                        }
                        _ => {}
                    }
                }
            }

            let packet = loop {
                let Some(packet) = self.video_rx.try_receive() else {
                    continue;
                };

                if packet_serial != packet.serial {
                    decoder_ctx.flush();
                    packet_serial = packet.serial;
                }

                if packet_serial == self.video_rx.metadata.serial.load(Ordering::SeqCst) {
                    break packet;
                }
            };

            if let Err(error) = decoder_ctx.send_packet(&packet.packet) {
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
