use crate::player::render_host::{PlaybackSessionId, RenderSize};

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
    VideoSizeChanged(Option<RenderSize>),
    Buffering(bool),
    PositionChanged(f64),
    DurationChanged(f64),
    BufferedChanged(Option<f64>),
    HttpStreamBufferedChanged(Option<HttpStreamBufferProgress>),
    LoadFailed(String),
    Fatal(String),
}

#[derive(Debug)]
pub enum BackendError {
    EmptyUrl,
    Ffmpeg(String),
}

pub type Result<T> = std::result::Result<T, BackendError>;

impl std::fmt::Display for BackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyUrl => write!(f, "播放地址为空"),
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
            BackendError::Ffmpeg("解码失败".to_string()).to_string(),
            "解码失败"
        );
    }
}
