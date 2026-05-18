use std::{fmt, sync::Arc};

use gpui::RenderImage;

use crate::player::render_host::{PlaybackSessionId, RenderSize};

#[derive(Clone, Debug, PartialEq)]
pub struct PlaybackVideoInfo {
    pub decoder: String,
    pub size: RenderSize,
    pub frame_rate: Option<f64>,
    pub hardware_accelerated: bool,
}

#[derive(Clone)]
pub struct BackendSubtitleBitmap {
    pub image: Arc<RenderImage>,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub canvas_width: u32,
    pub canvas_height: u32,
}

impl fmt::Debug for BackendSubtitleBitmap {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BackendSubtitleBitmap")
            .field("image", &"<render-image>")
            .field("x", &self.x)
            .field("y", &self.y)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("canvas_width", &self.canvas_width)
            .field("canvas_height", &self.canvas_height)
            .finish()
    }
}

impl PartialEq for BackendSubtitleBitmap {
    fn eq(&self, other: &Self) -> bool {
        self.x == other.x
            && self.y == other.y
            && self.width == other.width
            && self.height == other.height
            && self.canvas_width == other.canvas_width
            && self.canvas_height == other.canvas_height
            && self.image == other.image
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct BackendSubtitleCue {
    pub text: String,
    pub bitmaps: Vec<BackendSubtitleBitmap>,
    pub start_nsecs: u64,
    pub end_nsecs: u64,
}

impl BackendSubtitleCue {
    pub fn has_content(&self) -> bool {
        !self.text.trim().is_empty() || !self.bitmaps.is_empty()
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HttpStreamBufferProgress {
    pub start_fraction: f64,
    pub end_fraction: f64,
}

#[derive(Debug)]
pub struct BackendEvent {
    pub session_id: PlaybackSessionId,
    pub kind: BackendEventKind,
}

impl BackendEvent {
    pub fn new(session_id: PlaybackSessionId, kind: BackendEventKind) -> Self {
        Self { session_id, kind }
    }
}

#[derive(Debug)]
pub enum BackendEventKind {
    Pause(bool),
    PlaybackRestart,
    PlaybackInfoChanged(Option<PlaybackVideoInfo>),
    VideoSizeChanged(Option<RenderSize>),
    Buffering(bool),
    PositionChanged(f64),
    DurationChanged(f64),
    BufferedChanged(Option<f64>),
    HttpStreamBufferedChanged(Option<HttpStreamBufferProgress>),
    SubtitleChanged(Option<BackendSubtitleCue>),
    LoadFailed(String),
    Fatal(String),
}

#[derive(Debug)]
pub enum BackendError {
    EmptyUrl,
    UnsupportedCommand(&'static str),
    Ffmpeg(String),
}

pub type Result<T> = std::result::Result<T, BackendError>;

impl std::fmt::Display for BackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyUrl => write!(f, "播放地址为空"),
            Self::UnsupportedCommand(command) => write!(f, "当前播放后端不支持{command}"),
            Self::Ffmpeg(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for BackendError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_error_displays_user_facing_messages() {
        assert_eq!(BackendError::EmptyUrl.to_string(), "播放地址为空");
        assert_eq!(
            BackendError::UnsupportedCommand("切换音轨").to_string(),
            "当前播放后端不支持切换音轨"
        );
        assert_eq!(
            BackendError::Ffmpeg("解码失败".to_string()).to_string(),
            "解码失败"
        );
    }
}
