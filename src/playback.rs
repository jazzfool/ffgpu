use crate::decode::Decoder;
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use ffmpeg_next::{self as ffn, sys as ff};
use std::{
    i64,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicU32, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

// the following playback code is loosely based on ffplay.c

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlayState {
    Playing = 0,
    Paused,
    Step,
}

pub(crate) struct PlaybackState {
    pub(crate) alive: AtomicBool,
    pub(crate) play_state: AtomicU8,
    pub(crate) is_eof: AtomicBool,
    pub(crate) current_pts: AtomicI64,
}

impl PlaybackState {
    pub fn new() -> Self {
        PlaybackState {
            alive: AtomicBool::new(true),
            play_state: AtomicU8::new(PlayState::Playing as u8),
            is_eof: AtomicBool::new(false),
            current_pts: AtomicI64::new(0),
        }
    }

    pub fn play_state(&self) -> PlayState {
        match self.play_state.load(Ordering::Relaxed) {
            0 => PlayState::Playing,
            1 => PlayState::Paused,
            2 => PlayState::Step,
            _ => unreachable!(),
        }
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
    SeekStream(i64),
}

pub(crate) struct ReadThread {
    decoder: Arc<Decoder>,
    pbs: Arc<PlaybackState>,
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
            let play_state = self.pbs.play_state();
            // TODO: for network streams
            /*if self.was_paused != paused {
                self.was_paused = paused;
                if let Err(error) = if self.was_paused {
                    format_ctx.pause()
                } else {
                    format_ctx.play()
                } {
                    dbg!(std::ffi::c_int::from(error));
                    log::error!("failed to play/pause stream: {}", error);
                }
            }*/

            while let Ok(message) = self.messages.try_recv() {
                match message {
                    ReadMessage::SeekStream(ts) => {
                        if let Err(error) = {
                            let err = unsafe {
                                ff::avformat_seek_file(
                                    format_ctx.as_mut_ptr(),
                                    -1,
                                    i64::MIN,
                                    ts,
                                    i64::MAX,
                                    ff::AVSEEK_FLAG_BACKWARD,
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

                            if false && play_state == PlayState::Paused {
                                self.pbs
                                    .play_state
                                    .store(PlayState::Step as _, Ordering::Relaxed);
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

            if play_state == PlayState::Playing && is_eof && self.frame_queue.queue_rx.is_empty() {
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

pub(crate) enum DecodeMessage {
    SkipToTimestamp(i64),
}

pub(crate) struct VideoThread {
    decoder: Arc<Decoder>,
    pbs: Arc<PlaybackState>,
    video_rx: PacketReceiver,
    frame_queue: FrameQueue,
    messages: Receiver<DecodeMessage>,
}

impl VideoThread {
    pub fn new(
        decoder: Arc<Decoder>,
        pbs: Arc<PlaybackState>,
        video_rx: PacketReceiver,
        frame_queue: FrameQueue,
        messages: Receiver<DecodeMessage>,
    ) -> Self {
        VideoThread {
            decoder,
            pbs,
            video_rx,
            frame_queue,
            messages,
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

        let mut skip_to_ts = None;

        'exit: while self.pbs.alive.load(Ordering::Relaxed) {
            while let Ok(message) = self.messages.try_recv() {
                match message {
                    DecodeMessage::SkipToTimestamp(ts) => {
                        skip_to_ts = Some(ts);
                    }
                }
            }

            let mut prev_frame = None;

            while self.pbs.alive.load(Ordering::Relaxed) {
                if self.frame_queue.free_rx.is_empty() {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }

                let frame = match decoder_ctx.receive_frame(&mut frame) {
                    Ok(_) => {
                        if let Some(pts) = frame.pts() {
                            let av_pts = unsafe {
                                ff::av_rescale_q(
                                    pts,
                                    self.decoder.query_info.time_base.into(),
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
                            decoder_ctx.flush();
                            break;
                        }
                    }
                    Err(ffn::Error::Other { errno: ff::EAGAIN }) => {
                        break;
                    }
                    _ => None,
                };

                if let Some(frame) = frame {
                    let mut step = false;
                    if let Some(pts) = frame.pts() {
                        self.pbs.current_pts.store(pts, Ordering::SeqCst);
                        if skip_to_ts.is_some() {
                            skip_to_ts = None;
                            step = self.pbs.play_state() == PlayState::Paused;
                        }
                    }

                    self.frame_queue.send(frame, packet_serial);
                    if step {
                        self.pbs
                            .play_state
                            .store(PlayState::Step as _, Ordering::Relaxed);
                    }
                }
            }

            let packet = loop {
                if !self.pbs.alive.load(Ordering::Relaxed) {
                    break 'exit;
                }

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
