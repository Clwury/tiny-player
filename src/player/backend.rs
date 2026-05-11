mod events;
mod ffmpeg;

pub use events::{BackendError, BackendEvent, Result};
pub use ffmpeg::FfmpegBackend;
