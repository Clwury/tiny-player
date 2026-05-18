mod events;
mod ffmpeg;

use crate::player::render_host::FrameSlot;

use super::tracks::{PlaybackTrack, PlaybackTrackSelection};
pub use events::{
    BackendError, BackendEvent, BackendEventKind, BackendSubtitleBitmap, BackendSubtitleCue,
    HttpStreamBufferProgress, PlaybackVideoInfo, Result,
};
pub use ffmpeg::FfmpegBackend;

#[derive(Clone, Debug, PartialEq)]
pub struct BackendLoadRequest {
    pub url: String,
    pub http_headers: Vec<(String, String)>,
    pub content_length: Option<u64>,
    pub selected_tracks: PlaybackTrackSelection,
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
        position_seconds: f64,
    },
    #[allow(dead_code)]
    SetSubtitleTrack {
        track: Option<PlaybackTrack>,
        position_seconds: f64,
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
    fn set_audio_track(
        &mut self,
        _track_index: Option<usize>,
        _position_seconds: f64,
    ) -> Result<()> {
        Err(BackendError::UnsupportedCommand("切换音轨"))
    }
    fn set_subtitle_track(
        &mut self,
        _track: Option<PlaybackTrack>,
        _position_seconds: f64,
    ) -> Result<()> {
        Err(BackendError::UnsupportedCommand("切换字幕轨"))
    }
    fn poll_events(&mut self) -> Vec<BackendEvent>;
    fn frame_slot(&self) -> FrameSlot;

    fn command(&mut self, command: BackendCommand) -> Result<()> {
        match command {
            BackendCommand::Load(request) => self.load(request),
            BackendCommand::Seek { position_seconds } => self.seek(position_seconds),
            BackendCommand::Pause => self.pause(),
            BackendCommand::Resume => self.resume(),
            BackendCommand::Stop => self.stop(),
            BackendCommand::SetAudioTrack {
                track_index,
                position_seconds,
            } => self.set_audio_track(track_index, position_seconds),
            BackendCommand::SetSubtitleTrack {
                track,
                position_seconds,
            } => self.set_subtitle_track(track, position_seconds),
            BackendCommand::SetPlaybackRate { .. } => {
                Err(BackendError::UnsupportedCommand("调整播放速度"))
            }
        }
    }
}
