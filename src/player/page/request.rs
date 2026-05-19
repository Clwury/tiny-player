use super::*;

#[derive(Clone)]
pub struct PlaybackRequest {
    pub title: SharedString,
    pub url: String,
    pub http_headers: Vec<(String, String)>,
    pub content_length: Option<u64>,
    pub audio_tracks: Vec<PlaybackTrack>,
    pub subtitle_tracks: Vec<PlaybackTrack>,
    pub selected_tracks: PlaybackTrackSelection,
}

impl fmt::Debug for PlaybackRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PlaybackRequest")
            .field("title", &self.title)
            .field("url", &"<redacted>")
            .field("http_headers", &self.http_headers.len())
            .field("content_length", &self.content_length)
            .field("audio_tracks", &self.audio_tracks.len())
            .field("subtitle_tracks", &self.subtitle_tracks.len())
            .field("selected_tracks", &self.selected_tracks)
            .finish()
    }
}
