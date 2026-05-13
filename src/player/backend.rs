mod events;
mod ffmpeg;

use crate::player::render_host::FrameSlot;

pub use events::{BackendError, BackendEvent, BackendEventKind, HttpStreamBufferProgress, Result};
pub use ffmpeg::FfmpegBackend;

#[derive(Clone, Debug, PartialEq)]
pub struct BackendLoadRequest {
    pub url: String,
    pub http_headers: Vec<(String, String)>,
    pub content_length: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum BackendCommand {
    Load(BackendLoadRequest),
    Seek {
        position_seconds: f64,
    },
    Pause,
    Resume,
    #[allow(dead_code)]
    Stop,
    #[allow(dead_code)]
    SetAudioTrack {
        track_index: Option<usize>,
    },
    #[allow(dead_code)]
    SetSubtitleTrack {
        track_index: Option<usize>,
    },
    #[allow(dead_code)]
    SetPlaybackRate {
        rate: f64,
    },
}

pub trait BackendControl {
    fn load(&mut self, request: BackendLoadRequest) -> Result<()>;
    fn seek(&mut self, position_seconds: f64) -> Result<()>;
    fn pause(&mut self) -> Result<()>;
    fn resume(&mut self) -> Result<()>;
    #[allow(dead_code)]
    fn stop(&mut self) -> Result<()>;
    fn poll_events(&mut self) -> Vec<BackendEvent>;
    fn frame_slot(&self) -> FrameSlot;

    fn command(&mut self, command: BackendCommand) -> Result<()> {
        match command {
            BackendCommand::Load(request) => self.load(request),
            BackendCommand::Seek { position_seconds } => self.seek(position_seconds),
            BackendCommand::Pause => self.pause(),
            BackendCommand::Resume => self.resume(),
            BackendCommand::Stop => self.stop(),
            BackendCommand::SetAudioTrack { .. } => {
                Err(BackendError::UnsupportedCommand("切换音轨"))
            }
            BackendCommand::SetSubtitleTrack { .. } => {
                Err(BackendError::UnsupportedCommand("切换字幕轨"))
            }
            BackendCommand::SetPlaybackRate { .. } => {
                Err(BackendError::UnsupportedCommand("调整播放速度"))
            }
        }
    }
}
