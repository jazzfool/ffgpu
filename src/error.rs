use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    FFmpeg(ffmpeg_next::Error),
    InvalidStream,
    MissingCodec(&'static str),
    HardwareContext,
    Unknown,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::FFmpeg(error) => write!(f, "FFmpeg error: {}", error),
            Error::InvalidStream => f.write_str("failed to select stream"),
            Error::MissingCodec(codec_name) => {
                write!(f, "failed to find codec {} for stream", codec_name)
            }
            Error::HardwareContext => f.write_str("hardware context error"),
            Error::Unknown => f.write_str("unknown error"),
        }
    }
}

impl From<ffmpeg_next::Error> for Error {
    fn from(error: ffmpeg_next::Error) -> Self {
        Error::FFmpeg(error)
    }
}
