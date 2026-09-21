use crate::{emby::MediaItem, player::premiere_day};

use super::{SeriesDetailState, video_metadata::video_quality_label};

pub(super) fn hero_metadata_label(detail: &SeriesDetailState) -> Option<String> {
    // A season switch clears the episode selection until its response arrives.
    // Keep the reserved row empty instead of flashing series/next-up metadata.
    let item = detail.selected_playback_item()?;
    let source = detail.selected_media_source();
    let runtime = source
        .and_then(|source| source.run_time_ticks)
        .filter(|ticks| *ticks > 0)
        .or(item.run_time_ticks);
    let date = item
        .premiere_date
        .as_deref()
        .and_then(premiere_day)
        .map(ToString::to_string)
        .or_else(|| {
            item.production_year
                .filter(|year| *year > 0)
                .map(|year| year.to_string())
        });
    let streams = source
        .map(|source| source.media_streams.as_deref().unwrap_or_default())
        .or(item.media_streams.as_deref())
        .unwrap_or_default();
    let parts = [
        runtime.and_then(runtime_label),
        date,
        video_quality_label(streams),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

pub(super) fn movie_overview(item: &MediaItem) -> Option<String> {
    let overview = item
        .overview
        .as_deref()?
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    (!overview.is_empty()).then_some(overview)
}

fn runtime_label(ticks: u64) -> Option<String> {
    let seconds = ticks / 10_000_000;
    let minutes = seconds / 60;
    let hours = minutes / 60;
    match (hours, minutes % 60) {
        (0, 0) => (seconds > 0).then(|| format!("{seconds}秒")),
        (0, minutes) => Some(format!("{minutes}分钟")),
        (hours, 0) => Some(format!("{hours}小时")),
        (hours, minutes) => Some(format!("{hours}小时{minutes}分钟")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::home::LoadState;
    use serde_json::json;

    fn detail(item_type: &str) -> SeriesDetailState {
        SeriesDetailState::from_user_item(
            &serde_json::from_value(json!({
                "Id": "item-1", "Name": "Title", "Type": item_type
            }))
            .unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn hero_metadata_follows_selected_movie_versions_and_episodes() {
        let mut movie = detail("Movie");
        movie.item = Some(
            serde_json::from_value(json!({
                "Id": "item-1", "Name": "Movie", "Type": "Movie",
                "RunTimeTicks": 72_000_000_000_u64,
                "PremiereDate": "2024-02-29T23:30:00-08:00",
                "MediaSources": [
                    {"Id": "hd", "Type": "Default", "MediaStreams": [
                        {"Type": "Video", "Width": 1920, "Height": 800, "VideoRange": "SDR"}
                    ]},
                    {"Id": "uhd", "RunTimeTicks": 79_200_000_000_u64, "MediaStreams": [
                        {"Type": "Video", "Width": 3840, "Height": 1600, "VideoRange": "HDR10"}
                    ]}
                ]
            }))
            .unwrap(),
        );
        assert_eq!(
            hero_metadata_label(&movie).as_deref(),
            Some("2小时 · 2024-02-29 · 1080p SDR")
        );
        movie.select_media_source(1);
        assert_eq!(
            hero_metadata_label(&movie).as_deref(),
            Some("2小时12分钟 · 2024-02-29 · 4K HDR10")
        );

        let mut series = detail("Series");
        series.episodes = Some(
            serde_json::from_value(json!({"Items": [
                {"Id": "episode-1", "Name": "First", "RunTimeTicks": 14_550_000_000_u64,
                 "PremiereDate": "2026-09-20T00:00:00Z"},
                {"Id": "episode-2", "Name": "Second", "RunTimeTicks": 15_000_000_000_u64,
                 "PremiereDate": "2026-09-21T00:00:00Z"}
            ]}))
            .unwrap(),
        );
        assert_eq!(
            hero_metadata_label(&series).as_deref(),
            Some("24分钟 · 2026-09-20")
        );
        series.selected_episode_id = Some("episode-2".into());
        assert_eq!(
            hero_metadata_label(&series).as_deref(),
            Some("25分钟 · 2026-09-21")
        );
    }

    #[test]
    fn season_changes_do_not_flash_series_or_other_season_metadata() {
        for has_next_up in [false, true] {
            let mut series = detail("Series");
            series.item = Some(
                serde_json::from_value(json!({
                    "Id": "item-1", "Name": "Series", "Type": "Series",
                    "PremiereDate": "2024-01-01T00:00:00Z", "ProductionYear": 2024
                }))
                .unwrap(),
            );
            series.selected_season_id = Some("season-1".into());
            series.episodes = Some(
                serde_json::from_value(json!({"Items": [{
                    "Id": "episode-1", "Name": "First", "SeasonId": "season-1",
                    "RunTimeTicks": 14_550_000_000_u64, "PremiereDate": "2024-01-02T00:00:00Z"
                }]}))
                .unwrap(),
            );
            series.choose_episode_from_loaded_episodes();
            assert_eq!(
                hero_metadata_label(&series).as_deref(),
                Some("24分钟 · 2024-01-02")
            );
            if has_next_up {
                series.next_up = series.episodes.clone();
            }

            series.selected_season_id = Some("season-2".into());
            series.reset_episode_selection();
            for state in [
                LoadState::Idle,
                LoadState::Loading,
                LoadState::Failed,
                LoadState::Loaded,
            ] {
                series.effects.episodes = state;
                if state == LoadState::Loaded {
                    series.episodes = Some(serde_json::from_value(json!({"Items": []})).unwrap());
                }
                assert_eq!(
                    hero_metadata_label(&series),
                    None,
                    "state={state:?}, next_up={has_next_up}"
                );
            }

            series.episodes = Some(
                serde_json::from_value(json!({"Items": [{
                    "Id": "episode-2", "Name": "Second", "SeasonId": "season-2",
                    "RunTimeTicks": 15_000_000_000_u64, "PremiereDate": "2026-09-21T00:00:00Z",
                    "MediaSources": [{"Id": "source-2", "MediaStreams": [
                        {"Type": "Video", "Width": 3840, "Height": 1600, "VideoRange": "HDR10"}
                    ]}]
                }]}))
                .unwrap(),
            );
            series.choose_episode_from_loaded_episodes();
            assert_eq!(
                hero_metadata_label(&series).as_deref(),
                Some("25分钟 · 2026-09-21 · 4K HDR10")
            );
        }
    }

    #[test]
    fn hero_metadata_omits_invalid_fields_and_uses_item_streams_without_sources() {
        let mut movie = detail("Movie");
        for (fields, expected) in [
            (json!({"RunTimeTicks": 0, "PremiereDate": "unknown"}), None),
            (json!({"RunTimeTicks": 450_000_000_u64}), Some("45秒")),
            (
                json!({"PremiereDate": "2023-02-29", "ProductionYear": 2023}),
                Some("2023"),
            ),
            (
                json!({"MediaStreams": [{"Type": "Video", "Width": 3840, "Height": 1600,
                                     "VideoRange": "HDR10+"}]}),
                Some("4K HDR10+"),
            ),
        ] {
            let mut item = json!({"Id": "item-1", "Name": "Movie"});
            item.as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            movie.item = Some(serde_json::from_value(item).unwrap());
            assert_eq!(hero_metadata_label(&movie).as_deref(), expected);
        }
    }
}
