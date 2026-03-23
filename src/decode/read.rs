use crate::{
    SeekMode,
    decode::{DecoderState, PlayState, audio, video},
    error::Result,
};
use crossbeam_channel::Receiver;
use ffmpeg_next::{self as ffn, sys as ff};
use std::{
    path::Path,
    sync::{Arc, atomic::Ordering},
    thread::JoinHandle,
    time::Duration,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Metadata {
    pub duration: Duration,
}

pub(crate) struct Input {
    pub format_ctx: ffn::format::context::Input,
    pub metadata: Metadata,
}

impl Input {
    pub fn open<P>(path: &P) -> Result<Self>
    where
        P: AsRef<Path> + ?Sized,
    {
        ffn::init()?;
        let format_ctx = ffn::format::input(path)?;

        let duration =
            format_ctx.duration() as f64 / f64::from(ffn::Rational::from(ff::AV_TIME_BASE_Q));
        let metadata = Metadata {
            duration: Duration::from_secs_f64(duration),
        };

        Ok(Input {
            format_ctx,
            metadata,
        })
    }
}

#[derive(Debug)]
pub(crate) enum ReadMessage {
    SeekStream { ts: i64, mode: SeekMode },
}

pub(crate) struct ReadThread {
    input: Input,
    state: Arc<DecoderState>,
    messages: Receiver<ReadMessage>,
}

impl ReadThread {
    pub fn new(input: Input, pbs: Arc<DecoderState>, messages: Receiver<ReadMessage>) -> Self {
        ReadThread {
            input,
            state: pbs,
            messages,
        }
    }

    fn run_thread(&mut self) {
        const MAX_QUEUE_SIZE: usize = 15 * 1024 * 1024;

        while self.state.alive.load(Ordering::Relaxed) {
            let play_state = self.state.play_state();
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

            let video_stream = self.state.video_stream.load().clone();
            let audio_stream = self.state.audio_stream.load().clone();

            while let Ok(message) = self.messages.try_recv() {
                match message {
                    ReadMessage::SeekStream { ts, mode } => {
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
                            self.state.is_eof.store(false, Ordering::SeqCst);

                            video_stream.packets.flush();
                            video_stream
                                .packets
                                .push_null(ffn::Packet::empty(), video_stream.metadata.index);

                            audio_stream.packets.flush();
                            audio_stream
                                .packets
                                .push_null(ffn::Packet::empty(), audio_stream.metadata.index);

                            match mode {
                                SeekMode::Accurate => {
                                    let _ = video_stream
                                        .messages
                                        .send(video::Message::SkipToTimestamp(ts));
                                    let _ = audio_stream
                                        .messages
                                        .send(audio::Message::SkipToTimestamp(ts));
                                }
                                _ => {}
                            }

                            if play_state == PlayState::Paused {
                                self.state
                                    .play_state
                                    .store(PlayState::Step as _, Ordering::Relaxed);
                            }
                        }
                    }
                }
            }

            if (video_stream.packets.tx.len() + audio_stream.packets.tx.len()/*+ self.subtitle_tx.packets.len()*/)
                > MAX_QUEUE_SIZE
                || (video_stream
                    .packets
                    .has_enough_packets(video_stream.metadata.time_base)
                    && audio_stream
                        .packets
                        .has_enough_packets(audio_stream.metadata.time_base))
            {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }

            let is_eof = self.state.is_eof.load(Ordering::SeqCst);

            if play_state == PlayState::Playing && is_eof && video_stream.frames.queue_rx.is_empty()
            {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }

            let mut packet = ffn::Packet::empty();
            match packet.read(&mut self.input.format_ctx) {
                Ok(_) => {
                    self.state.is_eof.store(false, Ordering::SeqCst);
                }
                Err(error) => {
                    if !is_eof
                        && (error == ffn::Error::Eof
                            || unsafe { ff::avio_feof((*self.input.format_ctx.as_ptr()).pb) != 0 })
                    {
                        // flush
                        video_stream
                            .packets
                            .push_null(ffn::Packet::empty(), video_stream.metadata.index);
                        audio_stream
                            .packets
                            .push_null(ffn::Packet::empty(), audio_stream.metadata.index);
                        // self.subtitle_q.push_null();

                        self.state.is_eof.store(true, Ordering::SeqCst);
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
            if stream_index == video_stream.metadata.index {
                video_stream.packets.push(packet);
            } else if stream_index == audio_stream.metadata.index {
                audio_stream.packets.push(packet);
            }

            // TODO: handle subtitle packets
        }

        // force the other threads to wake up in order to exit from alive=false
        let video_stream = self.state.video_stream.load().clone();
        let audio_stream = self.state.audio_stream.load().clone();
        video_stream
            .packets
            .push_null(ffn::Packet::empty(), video_stream.metadata.index);
        audio_stream
            .packets
            .push_null(ffn::Packet::empty(), audio_stream.metadata.index);
    }

    pub fn run(mut self) -> JoinHandle<()> {
        std::thread::spawn(move || {
            self.run_thread();
        })
    }
}

unsafe impl Send for ReadThread {}
unsafe impl Sync for ReadThread {}
