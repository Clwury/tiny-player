use super::*;

pub const EMBY_TICKS_PER_SECOND: u64 = 10_000_000;

#[derive(Clone)]
pub struct PlaybackQueueItem {
    pub item_id: String,
    pub title: SharedString,
    pub series_id: Option<String>,
    pub season_id: Option<String>,
    pub run_time_ticks: Option<u64>,
    pub playback_position_ticks: Option<u64>,
    pub media_sources: Vec<crate::emby::MediaSource>,
}

impl fmt::Debug for PlaybackQueueItem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PlaybackQueueItem")
            .field("item_id", &self.item_id)
            .field("title", &self.title)
            .field("series_id", &self.series_id)
            .field("season_id", &self.season_id)
            .field("run_time_ticks", &self.run_time_ticks)
            .field("playback_position_ticks", &self.playback_position_ticks)
            .field("media_sources", &self.media_sources.len())
            .finish()
    }
}

#[derive(Clone, Debug)]
pub struct PlaybackQueue {
    pub items: Vec<PlaybackQueueItem>,
    pub current_index: usize,
}

impl PlaybackQueue {
    pub fn new(items: Vec<PlaybackQueueItem>, current_index: usize) -> Self {
        let current_index = if items.is_empty() {
            0
        } else {
            current_index.min(items.len() - 1)
        };
        Self {
            items,
            current_index,
        }
    }

    pub fn current(&self) -> Option<&PlaybackQueueItem> {
        self.items.get(self.current_index)
    }

    pub fn previous_index(&self) -> Option<usize> {
        self.current_index.checked_sub(1)
    }

    pub fn next_index(&self) -> Option<usize> {
        let next = self.current_index.checked_add(1)?;
        (next < self.items.len()).then_some(next)
    }

    pub fn playlist_item_id(index: usize) -> String {
        format!("playlistItem{index}")
    }

    pub fn report_items(&self) -> Vec<crate::emby::PlaybackQueueReportItem> {
        self.items
            .iter()
            .enumerate()
            .map(|(index, item)| crate::emby::PlaybackQueueReportItem {
                id: item.item_id.clone(),
                playlist_item_id: Self::playlist_item_id(index),
            })
            .collect()
    }
}

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

pub fn playback_initial_position_seconds(
    playback_position_ticks: Option<u64>,
    run_time_ticks: Option<u64>,
) -> f64 {
    let Some(position_ticks) = playback_position_ticks.filter(|ticks| *ticks > 0) else {
        return 0.0;
    };
    if run_time_ticks.is_some_and(|runtime| runtime == 0 || position_ticks >= runtime) {
        return 0.0;
    }

    let seconds = position_ticks as f64 / EMBY_TICKS_PER_SECOND as f64;
    if seconds.is_finite() && seconds > 0.0 {
        seconds
    } else {
        0.0
    }
}

pub(crate) fn preferred_playback_media_source(
    sources: &[crate::emby::MediaSource],
) -> Option<&crate::emby::MediaSource> {
    let valid = |source: &&crate::emby::MediaSource| {
        source.id.as_deref().is_some_and(|id| !id.trim().is_empty())
    };
    sources
        .iter()
        .filter(valid)
        .find(|source| source.is_default_source())
        .or_else(|| {
            sources
                .iter()
                .filter(valid)
                .find(|source| source.has_default_video_stream())
        })
        .or_else(|| sources.iter().find(valid))
}

pub(crate) fn playback_audio_tracks_for_source(
    source: &crate::emby::MediaSource,
) -> Vec<PlaybackTrack> {
    source
        .audio_streams()
        .into_iter()
        .enumerate()
        .filter_map(|(index, stream)| {
            let stream_index = usize::try_from(stream.index?).ok()?;
            Some(PlaybackTrack::new(
                stream_index,
                stream.audio_label(index),
                stream.is_external.unwrap_or(false),
            ))
        })
        .collect()
}

pub(crate) fn playback_subtitle_tracks_for_source(
    source: &crate::emby::MediaSource,
    server: &crate::server::CachedServer,
    item_id: &str,
    media_source_id: &str,
) -> Vec<PlaybackTrack> {
    source
        .subtitle_streams()
        .into_iter()
        .enumerate()
        .filter_map(|(index, stream)| {
            let stream_index = usize::try_from(stream.index?).ok()?;
            let external_url =
                playback_subtitle_external_url(stream, server, item_id, media_source_id);
            Some(
                PlaybackTrack::new(
                    stream_index,
                    stream.display_title_label(index),
                    stream.is_external.unwrap_or(false),
                )
                .with_external_url(external_url)
                .with_codec(stream.codec.clone()),
            )
        })
        .collect()
}

pub(crate) fn default_playback_track_selection(
    source: &crate::emby::MediaSource,
    subtitle_tracks: &[PlaybackTrack],
) -> PlaybackTrackSelection {
    let default_audio_stream_index = source.audio_streams().into_iter().find_map(|stream| {
        stream
            .index
            .and_then(|index| usize::try_from(index).ok())
            .filter(|_| stream.is_default.unwrap_or(false))
    });
    let audio_stream_index = default_audio_stream_index.or_else(|| {
        source
            .audio_streams()
            .into_iter()
            .find_map(|stream| stream.index.and_then(|index| usize::try_from(index).ok()))
    });
    let selected_subtitle = source
        .preferred_subtitle_stream_position()
        .and_then(|position| {
            playback_subtitle_track_at_position(source, subtitle_tracks, position)
        });

    let mut selection = PlaybackTrackSelection {
        audio_stream_index,
        default_audio_stream_index,
        ..Default::default()
    };
    selection.set_subtitle_track(selected_subtitle);
    selection
}

pub(crate) fn playback_subtitle_track_at_position<'a>(
    source: &crate::emby::MediaSource,
    subtitle_tracks: &'a [PlaybackTrack],
    position: usize,
) -> Option<&'a PlaybackTrack> {
    let stream_index = source
        .subtitle_streams()
        .get(position)?
        .index
        .and_then(|index| usize::try_from(index).ok())?;
    subtitle_tracks
        .iter()
        .find(|track| track.stream_index == stream_index)
}

fn playback_subtitle_external_url(
    stream: &crate::emby::MediaStream,
    server: &crate::server::CachedServer,
    item_id: &str,
    media_source_id: &str,
) -> Option<String> {
    if !is_external_subtitle_stream(stream) {
        return None;
    }

    let delivery_url = stream
        .delivery_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| fallback_external_subtitle_delivery_url(stream, item_id, media_source_id))?;
    let mut url = crate::emby::playback::resolve_direct_stream_url(server, &delivery_url).ok()?;
    if !url.query_pairs().any(|(name, _)| name == "api_key")
        && let Some(access_token) = server
            .access_token
            .as_deref()
            .filter(|token| !token.is_empty())
    {
        url.query_pairs_mut().append_pair("api_key", access_token);
    }
    Some(url.to_string())
}

fn is_external_subtitle_stream(stream: &crate::emby::MediaStream) -> bool {
    stream.is_external.unwrap_or(false)
        || stream
            .delivery_method
            .as_deref()
            .is_some_and(|method| method.eq_ignore_ascii_case("External"))
}

fn fallback_external_subtitle_delivery_url(
    stream: &crate::emby::MediaStream,
    item_id: &str,
    media_source_id: &str,
) -> Option<String> {
    let stream_index = stream.index?;
    let extension = external_subtitle_extension(stream.codec.as_deref())?;
    Some(format!(
        "/Videos/{item_id}/{media_source_id}/Subtitles/{stream_index}/0/Stream.{extension}"
    ))
}

fn external_subtitle_extension(codec: Option<&str>) -> Option<&'static str> {
    match codec?.trim().to_ascii_lowercase().as_str() {
        "ass" => Some("ass"),
        "ssa" => Some("ssa"),
        "subrip" | "srt" => Some("srt"),
        "vtt" | "webvtt" => Some("vtt"),
        "sub" => Some("sub"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use crate::emby::{MediaSource, MediaStream};
    use crate::server::{CachedServer, Protocol, ServerEndpoint};

    use super::*;

    #[test]
    fn playback_queue_reports_stable_string_playlist_items() {
        let queue = PlaybackQueue::new(
            vec![
                queue_item("episode-1"),
                queue_item("episode-2"),
                queue_item("episode-3"),
            ],
            1,
        );

        assert_eq!(queue.previous_index(), Some(0));
        assert_eq!(queue.next_index(), Some(2));
        assert_eq!(queue.report_items()[1].id, "episode-2");
        assert_eq!(queue.report_items()[1].playlist_item_id, "playlistItem1");
    }

    #[test]
    fn playback_queue_disables_out_of_bounds_neighbors() {
        let first = PlaybackQueue::new(vec![queue_item("episode-1")], 0);
        assert_eq!(first.previous_index(), None);
        assert_eq!(first.next_index(), None);

        let last = PlaybackQueue::new(vec![queue_item("episode-1"), queue_item("episode-2")], 1);
        assert_eq!(last.previous_index(), Some(0));
        assert_eq!(last.next_index(), None);
    }

    #[test]
    fn initial_position_resumes_only_before_known_runtime_end() {
        assert_eq!(playback_initial_position_seconds(None, None), 0.0);
        assert_eq!(playback_initial_position_seconds(Some(0), None), 0.0);
        assert_eq!(
            playback_initial_position_seconds(Some(120_000_000), Some(600_000_000)),
            12.0
        );
        assert_eq!(
            playback_initial_position_seconds(Some(600_000_000), Some(600_000_000)),
            0.0
        );
        assert_eq!(
            playback_initial_position_seconds(Some(700_000_000), Some(600_000_000)),
            0.0
        );
        assert_eq!(
            playback_initial_position_seconds(Some(120_000_000), None),
            12.0
        );
    }

    #[test]
    fn playback_request_debug_redacts_urls_headers_and_credentials() {
        let selected_tracks = PlaybackTrackSelection {
            subtitle_stream_index: Some(3),
            subtitle_external_url: Some(
                "https://example.com/subtitle.ass?api_key=secret-token".to_string(),
            ),
            ..PlaybackTrackSelection::default()
        };
        let request = PlaybackRequest {
            title: "Episode".into(),
            url: "https://example.com/video.mkv?api_key=secret-token".to_string(),
            http_headers: vec![("X-Emby-Token".to_string(), "secret-token".to_string())],
            content_length: Some(100),
            audio_tracks: Vec::new(),
            subtitle_tracks: Vec::new(),
            selected_tracks,
            initial_position_seconds: 12.0,
            queue: PlaybackQueue::new(vec![queue_item("episode-1")], 0),
            emby: EmbyPlaybackContext {
                client: crate::emby::EmbyClient::new("device-1".to_string()).unwrap(),
                server: CachedServer {
                    id: "local-1".to_string(),
                    endpoint: ServerEndpoint {
                        protocol: Protocol::Https,
                        address: "example.com".to_string(),
                        port: 443,
                        path: "/emby".to_string(),
                    },
                    username: "user".to_string(),
                    password: "secret-password".to_string(),
                    user_id: Some("user-1".to_string()),
                    server_id: Some("server-1".to_string()),
                    server_name: Some("Server".to_string()),
                    access_token: Some("secret-token".to_string()),
                    item_counts: None,
                    added_at_unix: 1,
                },
                item_id: "episode-1".to_string(),
                media_source_id: "source-1".to_string(),
                play_session_id: Some("session-1".to_string()),
                run_time_ticks: Some(100_000_000),
            },
        };

        let debug = format!("{request:?}");

        assert!(!debug.contains("secret-token"));
        assert!(!debug.contains("secret-password"));
        assert!(!debug.contains("video.mkv"));
        assert!(!debug.contains("subtitle.ass"));
        assert!(!debug.contains("Episode"));
        assert!(!debug.contains("initial_position_seconds"));
        assert!(debug.contains("episode-1"));
        assert!(debug.contains("source-1"));
        assert!(debug.contains("queue_length"));
        assert!(debug.contains("queue_index"));
    }

    #[test]
    fn default_subtitle_selection_skips_streams_without_indices() {
        let source = MediaSource {
            id: Some("source-1".to_string()),
            name: None,
            path: None,
            source_type: None,
            container: None,
            media_streams: Some(vec![
                MediaStream {
                    index: None,
                    stream_type: Some("Subtitle".to_string()),
                    display_title: None,
                    title: None,
                    language: None,
                    codec: Some("ass".to_string()),
                    delivery_url: None,
                    delivery_method: None,
                    is_external: None,
                    is_default: Some(true),
                    is_forced: None,
                    is_text_subtitle_stream: None,
                    supports_external_stream: None,
                },
                MediaStream {
                    index: Some(7),
                    stream_type: Some("Subtitle".to_string()),
                    display_title: None,
                    title: None,
                    language: None,
                    codec: Some("ass".to_string()),
                    delivery_url: None,
                    delivery_method: None,
                    is_external: None,
                    is_default: None,
                    is_forced: Some(true),
                    is_text_subtitle_stream: None,
                    supports_external_stream: None,
                },
            ]),
            default_subtitle_stream_index: None,
        };
        let tracks =
            playback_subtitle_tracks_for_source(&source, &debug_server(), "episode-1", "source-1");

        let selection = default_playback_track_selection(&source, &tracks);

        assert_eq!(selection.subtitle_stream_index, Some(7));
    }

    fn queue_item(item_id: &str) -> PlaybackQueueItem {
        PlaybackQueueItem {
            item_id: item_id.to_string(),
            title: item_id.to_string().into(),
            series_id: Some("series-1".to_string()),
            season_id: Some("season-1".to_string()),
            run_time_ticks: None,
            playback_position_ticks: None,
            media_sources: Vec::new(),
        }
    }

    fn debug_server() -> CachedServer {
        CachedServer {
            id: "local-1".to_string(),
            endpoint: ServerEndpoint {
                protocol: Protocol::Https,
                address: "example.com".to_string(),
                port: 443,
                path: "/emby".to_string(),
            },
            username: "user".to_string(),
            password: "secret-password".to_string(),
            user_id: Some("user-1".to_string()),
            server_id: Some("server-1".to_string()),
            server_name: Some("Server".to_string()),
            access_token: Some("secret-token".to_string()),
            item_counts: None,
            added_at_unix: 1,
        }
    }
}
