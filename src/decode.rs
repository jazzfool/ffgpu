pub(crate) mod audio;
pub(crate) mod frames;
pub(crate) mod read;
pub(crate) mod video;

use crate::decode::{audio::AudioStream, read::Metadata, video::VideoStream};
use arc_swap::ArcSwap;
use atomic_float::AtomicF64;
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use ffmpeg_next::{self as ffn, sys as ff};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicU32, Ordering},
};

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlayState {
    Playing = 0,
    Paused,
    Step,
}

pub(crate) struct DecoderState {
    pub metadata: Metadata,

    pub alive: AtomicBool,
    pub play_state: AtomicU8,
    pub is_eof: AtomicBool,
    pub current_pts: AtomicI64,

    pub video_stream: ArcSwap<VideoStream>,
    pub audio_stream: ArcSwap<AudioStream>,
}

impl DecoderState {
    pub fn new(metadata: Metadata, video: VideoStream, audio: AudioStream) -> Self {
        DecoderState {
            metadata,

            alive: AtomicBool::new(true),
            play_state: AtomicU8::new(PlayState::Playing as u8),
            is_eof: AtomicBool::new(false),
            current_pts: AtomicI64::new(0),

            video_stream: ArcSwap::new(Arc::new(video)),
            audio_stream: ArcSwap::new(Arc::new(audio)),
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

pub(crate) struct Packet {
    pub packet: ffn::Packet,
    pub serial: u32,
}

#[derive(Clone)]
pub(crate) struct PacketSender {
    pub metadata: Arc<PacketQueueMetadata>,
    pub rx: Receiver<Packet>,
    pub tx: Sender<Packet>,
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
    fn receive(&self) -> Option<Packet> {
        let Ok(recv) = self.rx.recv() else {
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
                ff::av_frame_unref(dst.frame.as_mut_ptr());
                ff::av_frame_move_ref(dst.frame.as_mut_ptr(), frame.as_mut_ptr());
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

    pub fn flush(&self) {
        while let Some(frame) = self.try_next() {
            self.release(frame);
        }
    }
}

unsafe impl Send for FrameQueue {}
unsafe impl Sync for FrameQueue {}

// complete atomic abuse
// but we really don't want to use locking primitives
// because the clock is updated in the audio callback
pub(crate) struct Clock {
    pub pts: AtomicF64,
    pub pts_drift: AtomicF64,
    pub last_updated: AtomicF64,
    pub speed: AtomicF64,
    pub serial: AtomicU32,
    pub paused: AtomicBool,
    pub queue: Arc<PacketQueueMetadata>,
}

impl Clock {
    pub const NO_SYNC_THRESHOLD: f64 = 10.;
    pub const SYNC_MIN: f64 = 0.04;
    pub const SYNC_MAX: f64 = 0.1;
    pub const FRAME_DUPLICATION_THRESHOLD: f64 = 0.1;

    pub fn new(queue: Arc<PacketQueueMetadata>) -> Self {
        let clock = Clock {
            pts: AtomicF64::new(0.),
            pts_drift: AtomicF64::new(0.),
            last_updated: AtomicF64::new(0.),
            speed: AtomicF64::new(1.),
            serial: AtomicU32::new(0),
            paused: AtomicBool::new(false),
            queue,
        };
        clock.set(f64::NAN, u32::MAX, None);
        clock
    }

    pub fn get(&self) -> Option<f64> {
        let queue_serial = self.queue.serial.load(Ordering::Relaxed);
        if self.serial.load(Ordering::Relaxed) != queue_serial {
            None
        } else if self.paused.load(Ordering::Relaxed) {
            let pts = self.pts.load(Ordering::Relaxed);
            if pts.is_nan() { None } else { Some(pts) }
        } else {
            let t = ffn::time::relative() as f64 / 1000000.;
            Some(
                self.pts_drift.load(Ordering::Relaxed) + t
                    - (t - self.last_updated.load(Ordering::Relaxed))
                        * (1. - self.speed.load(Ordering::Relaxed)),
            )
        }
    }

    pub fn set(&self, pts: f64, serial: u32, time: Option<f64>) {
        let time = time.unwrap_or_else(|| ffn::time::relative() as f64 / 1000000.);
        self.pts.store(pts, Ordering::Relaxed);
        self.last_updated.store(time, Ordering::Relaxed);
        self.pts_drift.store(pts - time, Ordering::Relaxed);
        self.serial.store(serial, Ordering::Relaxed);
    }

    pub fn sync_to_slave(&self, slave: &Clock) {
        let clock = self.get();
        let slave_clock = self.get();
        if let Some(slave_clock) = slave_clock
            && clock.is_none_or(|clock| (clock - slave_clock).abs() > Clock::NO_SYNC_THRESHOLD)
        {
            self.set(slave_clock, slave.serial.load(Ordering::Relaxed), None);
        }
    }
}

pub(crate) fn sink_thread(state: Arc<DecoderState>, packets: PacketReceiver) {
    while state.alive.load(Ordering::Relaxed) {
        packets.receive();
    }
}
