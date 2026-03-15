use crate::{
    decode::{DecoderState, FrameQueue, PacketSender, PlayState},
    error::{Error, Result},
};
use crossbeam_channel::Receiver;
use ffmpeg_next::{self as ffn, sys as ff};
use std::{
    path::Path,
    sync::{Arc, atomic::Ordering},
    thread::JoinHandle,
    time::Duration,
};

pub(crate) struct Input {
    pub format_ctx: ffn::format::context::Input,
}

impl Input {
    pub fn open<P>(path: &P) -> Result<Self>
    where
        P: AsRef<Path> + ?Sized,
    {
        ffn::init()?;
        let format_ctx = ffn::format::input(path)?;
        Ok(Input { format_ctx })
    }
}

#[derive(Debug)]
pub(crate) enum ReadMessage {
    SeekStream(i64),
}

pub(crate) struct ReadThread {
    input: Input,
    pbs: Arc<DecoderState>,
    video_tx: PacketSender,
    audio_tx: PacketSender,
    video_frame_queue: FrameQueue,
    audio_frame_queue: FrameQueue,
    messages: Receiver<ReadMessage>,
}

impl ReadThread {
    pub fn new(
        input: Input,
        pbs: Arc<DecoderState>,
        video_tx: PacketSender,
        audio_tx: PacketSender,
        video_frame_queue: FrameQueue,
        audio_frame_queue: FrameQueue,
        messages: Receiver<ReadMessage>,
    ) -> Self {
        ReadThread {
            input,
            pbs,
            video_tx,
            audio_tx,
            video_frame_queue,
            audio_frame_queue,
            messages,
        }
    }

    fn run_thread(&mut self) {
        const MAX_QUEUE_SIZE: usize = 15 * 1024 * 1024;

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

            let video_stream = self
                .pbs
                .video_stream
                .read()
                .expect("read video_stream")
                .clone();

            let audio_stream = self
                .pbs
                .audio_stream
                .read()
                .expect("read audio_stream")
                .clone();

            while let Ok(message) = self.messages.try_recv() {
                match message {
                    ReadMessage::SeekStream(ts) => {
                        if let Err(error) = {
                            let err = unsafe {
                                ff::avformat_seek_file(
                                    self.input.format_ctx.as_mut_ptr(),
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
                                .push_null(ffn::Packet::empty(), video_stream.index);

                            self.audio_tx.flush();
                            self.audio_tx
                                .push_null(ffn::Packet::empty(), audio_stream.index);

                            if play_state == PlayState::Paused {
                                self.pbs
                                    .play_state
                                    .store(PlayState::Step as _, Ordering::Relaxed);
                            }
                        }
                    }
                }
            }

            if (self.video_tx.tx.len() + self.audio_tx.tx.len()/*+ self.subtitle_tx.packets.len()*/)
                > MAX_QUEUE_SIZE
                || (self.video_tx.has_enough_packets(video_stream.time_base)
                    && self.audio_tx.has_enough_packets(audio_stream.time_base))
            {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }

            let is_eof = self.pbs.is_eof.load(Ordering::SeqCst);

            if play_state == PlayState::Playing
                && is_eof
                && self.video_frame_queue.queue_rx.is_empty()
            {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }

            let mut packet = ffn::Packet::empty();
            match packet.read(&mut self.input.format_ctx) {
                Ok(_) => {
                    self.pbs.is_eof.store(false, Ordering::SeqCst);
                }
                Err(error) => {
                    if !is_eof
                        && (error == ffn::Error::Eof
                            || unsafe { ff::avio_feof((*self.input.format_ctx.as_ptr()).pb) != 0 })
                    {
                        // flush
                        self.video_tx
                            .push_null(ffn::Packet::empty(), video_stream.index);
                        self.audio_tx
                            .push_null(ffn::Packet::empty(), audio_stream.index);
                        // self.subtitle_q.push_null();

                        self.pbs.is_eof.store(true, Ordering::SeqCst);
                    }

                    let avio_error = unsafe { (*(*self.input.format_ctx.as_ptr()).pb).error };
                    if avio_error != 0 {
                        log::error!("AVIOContext error {}", avio_error);
                    }

                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
            }

            let stream_index = packet.stream();
            if stream_index == video_stream.index {
                self.video_tx.push(packet);
            } else if stream_index == audio_stream.index {
                self.audio_tx.push(packet);
            }

            // TODO: handle subtitle packets
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
