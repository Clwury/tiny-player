use gpui::SharedString;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackTrackKind {
    Audio,
    Subtitle,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaybackTrack {
    pub stream_index: usize,
    pub label: SharedString,
    pub is_external: bool,
    pub external_url: Option<String>,
    pub codec: Option<String>,
}

impl PlaybackTrack {
    pub fn new(stream_index: usize, label: impl Into<SharedString>, is_external: bool) -> Self {
        Self {
            stream_index,
            label: label.into(),
            is_external,
            external_url: None,
            codec: None,
        }
    }

    pub fn with_external_url(mut self, external_url: Option<String>) -> Self {
        self.external_url = external_url;
        self
    }

    pub fn with_codec(mut self, codec: Option<String>) -> Self {
        self.codec = codec;
        self
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlaybackTrackSelection {
    pub audio_stream_index: Option<usize>,
    pub subtitle_stream_index: Option<usize>,
    pub subtitle_external_url: Option<String>,
    pub subtitle_codec: Option<String>,
}

impl PlaybackTrackSelection {
    pub fn set_subtitle_track(&mut self, track: Option<&PlaybackTrack>) {
        self.subtitle_stream_index = track.map(|track| track.stream_index);
        self.subtitle_external_url = track.and_then(|track| track.external_url.clone());
        self.subtitle_codec = track.and_then(|track| track.codec.clone());
    }
}
