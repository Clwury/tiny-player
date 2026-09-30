//! Playback IO port shared by detail launch and playback session effects.
use super::PlaybackTrack;
pub(crate) use super::reporting::PlaybackReport;
use crate::emby::MediaSource;

pub(crate) struct ResolvedPlayback {
    pub(crate) item_id: String,
    pub(crate) url: String,
    pub(crate) http_headers: Vec<(String, String)>,
    pub(crate) content_length: Option<u64>,
    pub(crate) media_source_id: String,
    pub(crate) play_session_id: Option<String>,
}

pub(crate) trait PlaybackGateway: Send + Sync {
    fn report(&self, report: &PlaybackReport) -> anyhow::Result<()>;
    fn resolve_source(
        &self,
        item_id: &str,
        media_source_id: &str,
    ) -> anyhow::Result<ResolvedPlayback>;
    fn subtitle_tracks(
        &self,
        source: &MediaSource,
        item_id: &str,
        media_source_id: &str,
    ) -> Vec<PlaybackTrack>;
}
