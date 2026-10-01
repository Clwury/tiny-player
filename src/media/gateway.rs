//! Source resolution and subtitle conversion, without reporting or page ownership.
use super::PlaybackTrack;
use crate::emby::MediaSource;

pub(crate) struct ResolvedPlayback {
    pub(crate) item_id: String,
    pub(crate) url: String,
    pub(crate) http_headers: Vec<(String, String)>,
    pub(crate) content_length: Option<u64>,
    pub(crate) media_source_id: String,
    pub(crate) play_session_id: Option<String>,
}

pub(crate) trait PlaybackSourceGateway: Send + Sync {
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
