//! Immutable launch payload exchanged between Home, app shell and playback.
//! GPUI's SharedString remains a presentation-facing title; selection and time
//! rules live in pure models, while authenticated URL assembly is an adapter.
use crate::media::PlaybackQueue;
use gpui::SharedString;
use std::fmt;
use tiny_playback::{PlaybackTrack, PlaybackTrackSelection};

#[derive(Clone)]
pub struct EmbyPlaybackContext {
    pub client: crate::emby::EmbyClient,
    pub server: crate::server::CachedServer,
    pub item_id: String,
    pub media_source_id: String,
    pub play_session_id: Option<String>,
    pub run_time_ticks: Option<u64>,
}

impl fmt::Debug for EmbyPlaybackContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EmbyPlaybackContext")
            .field("item_id", &self.item_id)
            .field("media_source_id", &self.media_source_id)
            .field(
                "has_play_session_id",
                &self
                    .play_session_id
                    .as_ref()
                    .is_some_and(|id| !id.is_empty()),
            )
            .field("run_time_ticks", &self.run_time_ticks)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct PlaybackRequest {
    pub title: SharedString,
    pub url: String,
    pub http_headers: Vec<(String, String)>,
    pub content_length: Option<u64>,
    pub audio_tracks: Vec<PlaybackTrack>,
    pub subtitle_tracks: Vec<PlaybackTrack>,
    pub selected_tracks: PlaybackTrackSelection,
    pub track_preference_key: crate::player::PlaybackTrackPreferenceKey,
    /// Commit a subtitle chosen in detail only after playback actually starts.
    pub remember_subtitle_on_start: bool,
    pub initial_position_seconds: f64,
    pub queue: PlaybackQueue,
    pub emby: EmbyPlaybackContext,
}

impl fmt::Debug for PlaybackRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PlaybackRequest")
            .field("item_id", &self.emby.item_id)
            .field("media_source_id", &self.emby.media_source_id)
            .field("queue_length", &self.queue.items.len())
            .field("queue_index", &self.queue.current_index)
            .field("has_play_session_id", &self.emby.play_session_id.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests;
