mod actions;
mod images;
mod render;
mod state;
mod video_sources;

use super::notification::{HOME_RESUME_DETAIL_NOTIFICATION_KEY, NotificationScope};
pub(super) use actions::PlayedRequest;
pub(crate) use state::{SeriesDetailSelectKind, SeriesDetailState};

use gpui::{AppContext as _, ClickEvent, Context, MouseDownEvent, SharedString, Window};

use crate::{
    emby::{
        MediaItem, MediaItems, ResumeItem, UserItem, UserItems, playback::resolve_direct_stream_url,
    },
    player::{
        EmbyPlaybackContext, PlaybackLanguagePreferences, PlaybackQueue, PlaybackQueueItem,
        PlaybackRequest, PlaybackTrack, PlaybackTrackSelection, playback_initial_position_seconds,
    },
    server::CachedServer,
};

use super::{
    HomeContent, HomeContentEvent, LoadState, WorkspaceIdentity,
    carousel::DETAIL_EPISODE_CARD_STEP_PX,
};

const DETAIL_ITEM_NOTIFICATION_KEY: &str = "detail:item";
const DETAIL_SIMILAR_NOTIFICATION_KEY: &str = "detail:similar";
const DETAIL_SEASONS_NOTIFICATION_KEY: &str = "detail:seasons";
const DETAIL_NEXT_UP_NOTIFICATION_KEY: &str = "detail:next-up";
const DETAIL_EPISODES_NOTIFICATION_KEY: &str = "detail:episodes";
const DETAIL_PLAYBACK_NOTIFICATION_KEY: &str = "detail:playback";

struct SelectedPlayback {
    detail_id: String,
    list_item_id: String,
    item_id: String,
    media_source_id: String,
    title: SharedString,
    audio_tracks: Vec<PlaybackTrack>,
    subtitle_tracks: Vec<PlaybackTrack>,
    selected_tracks: PlaybackTrackSelection,
    run_time_ticks: Option<u64>,
    playback_position_ticks: Option<u64>,
    queue: PlaybackQueue,
}

struct ResolvedPlayback {
    item_id: String,
    url: String,
    http_headers: Vec<(String, String)>,
    content_length: Option<u64>,
    media_source_id: String,
    play_session_id: Option<String>,
}

#[derive(Clone, Copy)]
struct DetailRequestRevisions {
    detail: u64,
    user_data: u64,
}

#[path = "detail/loading.rs"]
mod loading;
#[path = "detail/navigation.rs"]
mod navigation;
#[path = "detail/selection.rs"]
mod selection;

fn selected_playback(
    detail: &SeriesDetailState,
    server: &CachedServer,
    languages: PlaybackLanguagePreferences,
) -> Result<SelectedPlayback, String> {
    let item = detail
        .selected_playback_item()
        .ok_or_else(|| "请选择要播放的媒体".to_string())?;
    let source = detail
        .selected_media_source()
        .ok_or_else(|| "请选择视频源".to_string())?;
    let media_source_id = source
        .id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| "所选视频源缺少 ID，无法获取播放地址".to_string())?
        .to_string();
    let title = if detail.is_movie() {
        item.name.clone()
    } else {
        let series_name = detail
            .item
            .as_ref()
            .map(|item| item.name.clone())
            .unwrap_or_else(|| detail.title.clone());
        format!("{series_name} {}", item.episode_label())
    };

    let audio_tracks = playback_audio_tracks(source);
    let item_id = source.playback_item_id(&item.id);
    let subtitle_tracks = playback_subtitle_tracks(source, server, item_id, &media_source_id);
    let selected_tracks = playback_track_selection(detail, source, &subtitle_tracks, languages);
    let playback_position_ticks = detail.playback_position_ticks();
    let mut queue = playback_queue(detail, item, &title);
    if let Some(current) = queue.items.get_mut(queue.current_index) {
        current.playback_position_ticks = playback_position_ticks;
        current.media_sources = detail.selected_media_sources().unwrap_or_default().to_vec();
    }

    Ok(SelectedPlayback {
        detail_id: detail.series_id.clone(),
        list_item_id: item.id.clone(),
        item_id: item_id.to_string(),
        media_source_id,
        title: title.into(),
        audio_tracks,
        subtitle_tracks,
        selected_tracks,
        run_time_ticks: item.run_time_ticks,
        playback_position_ticks,
        queue,
    })
}

fn playback_queue(
    detail: &SeriesDetailState,
    selected_item: &MediaItem,
    selected_title: &str,
) -> PlaybackQueue {
    if detail.is_movie() {
        return PlaybackQueue::new(
            vec![playback_queue_item(
                selected_item,
                selected_title.to_string().into(),
                None,
                None,
            )],
            0,
        );
    }

    let series_name = detail
        .item
        .as_ref()
        .map(|item| item.name.as_str())
        .unwrap_or(detail.title.as_str());
    let selected_season_id = detail.selected_season_id.clone();
    let mut items = detail
        .episodes
        .as_ref()
        .map(|episodes| {
            episodes
                .items
                .iter()
                .filter(|episode| {
                    playback_queue_episode_is_valid(episode)
                        && episode.season_id.as_deref().is_none_or(|season_id| {
                            Some(season_id) == selected_season_id.as_deref()
                        })
                })
                .map(|episode| {
                    playback_queue_item(
                        episode,
                        format!("{series_name} {}", episode.episode_label()).into(),
                        Some(detail.series_id.clone()),
                        episode
                            .season_id
                            .clone()
                            .or_else(|| selected_season_id.clone()),
                    )
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let current_index = items
        .iter()
        .position(|item| item.item_id == selected_item.id);
    if let Some(current_index) = current_index {
        return PlaybackQueue::new(items, current_index);
    }

    items.clear();
    items.push(playback_queue_item(
        selected_item,
        selected_title.to_string().into(),
        Some(detail.series_id.clone()),
        selected_item.season_id.clone().or(selected_season_id),
    ));
    PlaybackQueue::new(items, 0)
}

fn playback_queue_episode_is_valid(item: &MediaItem) -> bool {
    !item.id.trim().is_empty()
        && item
            .item_type
            .as_deref()
            .is_none_or(|item_type| item_type.eq_ignore_ascii_case("Episode"))
        && item.media_sources.as_ref().is_some_and(|sources| {
            sources
                .iter()
                .any(|source| source.id.as_deref().is_some_and(|id| !id.trim().is_empty()))
        })
}

fn playback_queue_item(
    item: &MediaItem,
    title: SharedString,
    series_id: Option<String>,
    season_id: Option<String>,
) -> PlaybackQueueItem {
    PlaybackQueueItem {
        item_id: item.id.clone(),
        title,
        episode_label: item.episode_label().into(),
        overview: item.overview.clone(),
        primary_image_tag: item.primary_image_tag().map(str::to_string),
        series_id,
        season_id,
        run_time_ticks: item.run_time_ticks,
        playback_position_ticks: item.playback_position_ticks(),
        media_sources: item.media_sources.clone().unwrap_or_default(),
    }
}

fn apply_media_item_user_data_overrides(
    items: &mut [MediaItem],
    overrides: &std::collections::HashMap<String, crate::emby::UserItemData>,
) {
    for item in items {
        if let Some(data) = overrides.get(&item.id) {
            item.user_data = Some(data.clone());
        }
    }
}

fn playback_audio_tracks(source: &crate::emby::MediaSource) -> Vec<PlaybackTrack> {
    crate::player::playback_audio_tracks_for_source(source)
}

fn playback_subtitle_tracks(
    source: &crate::emby::MediaSource,
    server: &CachedServer,
    item_id: &str,
    media_source_id: &str,
) -> Vec<PlaybackTrack> {
    crate::player::playback_subtitle_tracks_for_source(source, server, item_id, media_source_id)
}

fn playback_track_selection(
    detail: &SeriesDetailState,
    source: &crate::emby::MediaSource,
    subtitle_tracks: &[PlaybackTrack],
    languages: PlaybackLanguagePreferences,
) -> PlaybackTrackSelection {
    let mut selection =
        crate::player::preferred_playback_track_selection(source, subtitle_tracks, languages);
    let selected_subtitle =
        detail
            .selected_subtitle_index(languages.subtitle)
            .and_then(|position| {
                crate::player::playback_subtitle_track_at_position(
                    source,
                    subtitle_tracks,
                    position,
                )
            });

    selection.set_subtitle_track(selected_subtitle);
    selection
}

#[cfg(test)]
mod tests {
    use crate::{
        emby::{MediaItems, MediaSource, MediaStream, UserItem},
        server::{Protocol, ServerEndpoint},
    };

    use super::*;

    fn server() -> CachedServer {
        CachedServer {
            id: "server-detail-test".to_string(),
            endpoint: ServerEndpoint {
                protocol: Protocol::Https,
                address: "example.com".to_string(),
                port: 443,
                path: "/emby".to_string(),
            },
            username: "luv".to_string(),
            password: "secret".to_string(),
            user_id: Some("user-1".to_string()),
            server_id: Some("server-1".to_string()),
            server_name: Some("Home".to_string()),
            access_token: Some("token".to_string()),
            item_counts: None,
            added_at_unix: 123,
        }
    }

    #[test]
    fn playback_queue_keeps_server_order_and_current_season_only() {
        let series: UserItem = serde_json::from_value(serde_json::json!({
            "Id": "series-1",
            "Name": "Series",
            "Type": "Series"
        }))
        .unwrap();
        let mut detail = SeriesDetailState::new_series(&series);
        detail.selected_season_id = Some("season-1".to_string());
        detail.episodes = Some(
            serde_json::from_value::<MediaItems>(serde_json::json!({
                "Items": [
                    {
                        "Id": "episode-2",
                        "Name": "Second",
                        "Type": "Episode",
                        "SeasonId": "season-1",
                        "MediaSources": [{ "Id": "source-2" }]
                    },
                    {
                        "Id": "episode-1",
                        "Name": "First",
                        "ParentIndexNumber": 1,
                        "IndexNumber": 1,
                        "Overview": "First episode overview",
                        "ImageTags": {"Primary": "first-cover"},
                        "Type": "Episode",
                        "SeasonId": "season-1",
                        "MediaSources": [{ "Id": "source-1" }]
                    },
                    {
                        "Id": "episode-next-season",
                        "Name": "Next Season",
                        "Type": "Episode",
                        "SeasonId": "season-2",
                        "MediaSources": [{ "Id": "source-3" }]
                    },
                    {
                        "Id": "episode-no-source",
                        "Name": "Unavailable",
                        "Type": "Episode",
                        "SeasonId": "season-1"
                    }
                ],
                "TotalRecordCount": 4
            }))
            .unwrap(),
        );
        detail.selected_episode_id = Some("episode-1".to_string());
        let selected = detail.selected_episode().unwrap();

        let queue = playback_queue(&detail, selected, "Series S1E1");

        assert_eq!(
            queue
                .items
                .iter()
                .map(|item| item.item_id.as_str())
                .collect::<Vec<_>>(),
            vec!["episode-2", "episode-1"]
        );
        assert_eq!(queue.current_index, 1);
        assert_eq!(queue.next_index(), None);
        let current = queue.current().unwrap();
        assert_eq!(current.episode_label.as_ref(), "S1E1: First");
        assert_eq!(current.overview.as_deref(), Some("First episode overview"));
        assert_eq!(current.primary_image_tag.as_deref(), Some("first-cover"));
    }

    #[test]
    fn detail_and_playback_follow_languages_without_overriding_manual_subtitles() {
        use crate::player::TrackLanguage;
        let movie: UserItem = serde_json::from_value(serde_json::json!({
            "Id": "movie-1", "Name": "Movie", "Type": "Movie"
        }))
        .unwrap();
        let mut detail = SeriesDetailState::from_user_item(&movie).unwrap();
        detail.item = Some(serde_json::from_value(serde_json::json!({
            "Id": "movie-1", "Name": "Movie", "Type": "Movie",
            "MediaSources": [{"Id": "source-1", "MediaStreams": [
                {"Index": 1, "Type": "Audio", "Language": "eng", "IsDefault": true},
                {"Index": 3, "Type": "Audio", "Language": "jpn"},
                {"Index": 4, "Type": "Subtitle", "Language": "eng", "DisplayTitle": "English", "IsDefault": true},
                {"Index": 7, "Type": "Subtitle", "Language": "chs", "DisplayTitle": "简体中文"}
            ]}]
        })).unwrap());
        detail.sync_media_source_selection();
        assert_eq!(
            detail.selected_subtitle_label(TrackLanguage::Default),
            "English"
        );
        let languages = PlaybackLanguagePreferences {
            audio: TrackLanguage::Japanese,
            subtitle: TrackLanguage::ChineseSimplified,
        };
        assert_eq!(
            detail.selected_subtitle_label(languages.subtitle),
            "简体中文"
        );
        let selected = selected_playback(&detail, &server(), languages).unwrap();
        assert_eq!(selected.selected_tracks.audio_stream_index, Some(3));
        assert_eq!(selected.selected_tracks.subtitle_stream_index, Some(7));

        detail.selected_subtitle_index = Some(0);
        detail.sync_media_source_selection();
        assert_eq!(
            detail.selected_subtitle_label(languages.subtitle),
            "English"
        );
        let selected = selected_playback(&detail, &server(), languages).unwrap();
        assert_eq!(selected.selected_tracks.subtitle_stream_index, Some(4));
    }

    #[test]
    fn playback_subtitle_tracks_resolve_external_ass_delivery_url() {
        let source = MediaSource {
            id: Some("mediasource_1126227".to_string()),
            item_id: None,
            name: None,
            path: None,
            source_type: None,
            container: None,
            media_streams: Some(vec![MediaStream {
                index: Some(3),
                stream_type: Some("Subtitle".to_string()),
                display_title: Some("(ASS)".to_string()),
                title: Some("chs&eng".to_string()),
                language: None,
                codec: Some("ass".to_string()),
                delivery_url: Some(
                    "/Videos/1126227/mediasource_1126227/Subtitles/3/0/Stream.ass?api_key=token"
                        .to_string(),
                ),
                delivery_method: Some("External".to_string()),
                is_external: Some(true),
                is_default: Some(false),
                is_forced: Some(false),
                is_text_subtitle_stream: Some(true),
                supports_external_stream: Some(true),
            }]),
            default_subtitle_stream_index: None,
        };

        let tracks = playback_subtitle_tracks(&source, &server(), "1126227", "mediasource_1126227");

        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].stream_index, 3);
        assert!(tracks[0].is_external);
        assert_eq!(tracks[0].codec.as_deref(), Some("ass"));
        assert_eq!(
            tracks[0].external_url.as_deref(),
            Some(
                "https://example.com/emby/Videos/1126227/mediasource_1126227/Subtitles/3/0/Stream.ass?api_key=token"
            )
        );
    }

    #[test]
    fn playback_subtitle_tracks_build_external_ass_url_when_delivery_url_missing() {
        let source = MediaSource {
            id: Some("mediasource_1126227".to_string()),
            item_id: None,
            name: None,
            path: None,
            source_type: None,
            container: None,
            media_streams: Some(vec![MediaStream {
                index: Some(3),
                stream_type: Some("Subtitle".to_string()),
                display_title: Some("(ASS)".to_string()),
                title: Some("chs&eng".to_string()),
                language: None,
                codec: Some("ass".to_string()),
                delivery_url: None,
                delivery_method: Some("External".to_string()),
                is_external: Some(true),
                is_default: Some(false),
                is_forced: Some(false),
                is_text_subtitle_stream: Some(true),
                supports_external_stream: Some(true),
            }]),
            default_subtitle_stream_index: None,
        };

        let tracks = playback_subtitle_tracks(&source, &server(), "1126227", "mediasource_1126227");

        assert_eq!(tracks.len(), 1);
        assert!(tracks[0].is_external);
        assert_eq!(
            tracks[0].external_url.as_deref(),
            Some(
                "https://example.com/emby/Videos/1126227/mediasource_1126227/Subtitles/3/0/Stream.ass?api_key=token"
            )
        );
    }

    #[test]
    fn playback_subtitle_tracks_keep_internal_ass_on_embedded_stream() {
        let source = MediaSource {
            id: Some("mediasource_1126227".to_string()),
            item_id: None,
            name: None,
            path: None,
            source_type: None,
            container: None,
            media_streams: Some(vec![MediaStream {
                index: Some(2),
                stream_type: Some("Subtitle".to_string()),
                display_title: Some("Chinese Simplified (默认 ASS)".to_string()),
                title: Some("Simplified Chinese (简体中文)".to_string()),
                language: Some("chi".to_string()),
                codec: Some("ass".to_string()),
                delivery_url: None,
                delivery_method: Some("Embed".to_string()),
                is_external: Some(false),
                is_default: Some(true),
                is_forced: Some(false),
                is_text_subtitle_stream: Some(true),
                supports_external_stream: Some(true),
            }]),
            default_subtitle_stream_index: None,
        };

        let tracks = playback_subtitle_tracks(&source, &server(), "1126227", "mediasource_1126227");

        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].stream_index, 2);
        assert!(!tracks[0].is_external);
        assert_eq!(tracks[0].codec.as_deref(), Some("ass"));
        assert_eq!(tracks[0].external_url, None);
    }

    #[test]
    fn playback_subtitle_tracks_keep_internal_subrip_on_embedded_stream() {
        let source = MediaSource {
            id: Some("mediasource_824061".to_string()),
            item_id: None,
            name: None,
            path: None,
            source_type: None,
            container: None,
            media_streams: Some(vec![MediaStream {
                index: Some(2),
                stream_type: Some("Subtitle".to_string()),
                display_title: Some("Chinese Simplified (默认 SUBRIP)".to_string()),
                title: Some("Chinese Simplified".to_string()),
                language: Some("chi".to_string()),
                codec: Some("subrip".to_string()),
                delivery_url: None,
                delivery_method: Some("Embed".to_string()),
                is_external: Some(false),
                is_default: Some(true),
                is_forced: Some(false),
                is_text_subtitle_stream: Some(true),
                supports_external_stream: Some(true),
            }]),
            default_subtitle_stream_index: None,
        };

        let tracks = playback_subtitle_tracks(&source, &server(), "824061", "mediasource_824061");

        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].stream_index, 2);
        assert!(!tracks[0].is_external);
        assert_eq!(tracks[0].codec.as_deref(), Some("subrip"));
        assert_eq!(tracks[0].external_url, None);
    }
}
