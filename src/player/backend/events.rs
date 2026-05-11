use crate::player::render_host::RenderSize;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HttpStreamBufferProgress {
    pub start_fraction: f64,
    pub end_fraction: f64,
}

#[derive(Debug)]
pub enum BackendEvent {
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
