use crate::emby::{MediaItems, ResumeItem, UserItem, UserItemData};

use super::*;

#[test]
fn selection_changes_report_only_the_needed_presentation_updates() {
    let mut detail = SeriesDetailModel::new_series(&user_item("series", "Series"));
    detail.selected_episode_id = Some("first".into());
    detail.episodes = Some(MediaItems {
        items: vec![media_item("first", "First"), media_item("second", "Second")],
        total_record_count: 2,
    });
    let same = detail.apply_selected_episode(Some("first".into()));
    assert!(!same.episode_changed);
    assert!(same.selection_unavailable);
    let changed = detail.apply_selected_episode(Some("second".into()));
    assert!(changed.episode_changed);
    assert!(changed.selection_unavailable);
    assert_eq!(detail.selected_episode_id.as_deref(), Some("second"));
    assert_eq!(detail.select_media_source(10), DetailChange::default());
    let reset = detail.reset_episode_selection();
    assert!(reset.episodes_reset);
    assert!(detail.episodes.is_none());
    assert!(detail.selected_episode_id.is_none());
}

#[test]
fn next_up_season_change_invalidates_previous_episode_request() {
    let mut detail = SeriesDetailModel::new_series(&user_item("series", "Series"));
    detail.selected_season_id = Some("old-season".into());
    detail.effects.episodes = LoadState::Loading;
    detail.episodes_request_season_id = Some("old-season".into());
    let mut next = media_item("next", "Next");
    next.season_id = Some("new-season".into());
    detail.next_up = Some(MediaItems {
        items: vec![next],
        total_record_count: 1,
    });
    let change = detail.apply_next_up_preference();
    assert!(change.episodes_reset);
    assert_eq!(detail.selected_season_id.as_deref(), Some("new-season"));
    assert_eq!(detail.preferred_episode_id.as_deref(), Some("next"));
    assert_eq!(detail.effects.episodes, LoadState::Idle);
    assert!(detail.episodes_request_season_id.is_none());
    assert!(!detail.apply_next_up_preference().episodes_reset);
}

fn user_item(id: &str, name: &str) -> UserItem {
    UserItem {
        id: id.to_string(),
        name: name.to_string(),
        item_type: Some("Series".to_string()),
        media_type: None,
        parent_id: None,
        series_id: None,
        series_name: None,
        index_number: None,
        parent_index_number: None,
        production_year: None,
        community_rating: None,
        image_tags: None,
        backdrop_image_tags: None,
        user_data: None,
        collection_type: None,
        primary_image_aspect_ratio: None,
        child_count: None,
        container: None,
        can_delete: None,
        provider_ids: None,
    }
}

fn media_item(id: &str, name: &str) -> MediaItem {
    MediaItem {
        id: id.to_string(),
        name: name.to_string(),
        item_type: Some("Episode".to_string()),
        server_id: None,
        production_year: None,
        premiere_date: None,
        run_time_ticks: None,
        index_number: None,
        parent_index_number: None,
        is_folder: None,
        community_rating: None,
        official_rating: None,
        genres: None,
        genre_items: None,
        overview: None,
        series_name: None,
        series_id: None,
        season_id: None,
        season_name: None,
        media_type: None,
        image_tags: None,
        backdrop_image_tags: None,
        parent_logo_item_id: None,
        parent_logo_image_tag: None,
        parent_backdrop_item_id: None,
        parent_backdrop_image_tags: None,
        series_primary_image_tag: None,
        media_sources: None,
        media_streams: None,
        people: None,
        studios: None,
        external_urls: None,
        user_data: None,
    }
}

fn resume_episode(id: &str, name: &str, series_id: &str, season_number: u32) -> ResumeItem {
    ResumeItem {
        id: id.to_string(),
        name: name.to_string(),
        item_type: Some("Episode".to_string()),
        parent_id: None,
        series_name: Some("示例剧集".to_string()),
        series_id: Some(series_id.to_string()),
        parent_index_number: Some(season_number),
        index_number: None,
        production_year: None,
        image_tags: None,
        backdrop_image_tags: None,
        parent_backdrop_item_id: None,
        parent_backdrop_image_tags: None,
        user_data: None,
    }
}

#[test]
fn chooses_next_up_episode_when_loaded_episodes_include_it() {
    let mut detail = SeriesDetailModel {
        kind: SeriesDetailKind::Series,
        origin: SeriesDetailOrigin::UserView,
        series_id: "series-1".to_string(),
        title: "Series".to_string(),
        effects: Default::default(),
        item: None,
        item_failed: None,
        seasons: None,
        seasons_failed: None,
        next_up: Some(MediaItems {
            items: vec![media_item("episode-2", "第二集")],
            total_record_count: 2,
        }),
        next_up_failed: None,
        resume_episode: None,
        resume_media_item_id: None,
        resume_media_sources: None,
        resume_video_version: None,
        episodes: Some(MediaItems {
            items: vec![
                media_item("episode-1", "第一集"),
                media_item("episode-2", "第二集"),
            ],
            total_record_count: 2,
        }),
        episodes_failed: None,
        episode_selection_warning: None,
        similar_items: None,
        similar_failed: None,
        playback_loading: false,
        playback_failed: None,
        selected_season_id: None,
        selected_episode_id: None,
        preferred_episode_id: Some("episode-2".to_string()),
        preferred_season_id_hint: None,
        selected_media_source_index: None,
        manual_video_version: None,
        pending_subtitle_choices: HashMap::new(),
        episodes_request_season_id: None,
    };

    detail.choose_episode_from_loaded_episodes();

    assert_eq!(detail.selected_episode_id.as_deref(), Some("episode-2"));
    assert!(detail.should_reveal_selected_episode());
}

#[test]
fn hero_episode_prefers_selected_episode_over_next_up() {
    let mut detail = SeriesDetailModel::new_series(&user_item("series-1", "Series"));
    detail.next_up = Some(MediaItems {
        items: vec![media_item("episode-2", "第二集")],
        total_record_count: 2,
    });
    detail.episodes = Some(MediaItems {
        items: vec![
            media_item("episode-1", "第一集"),
            media_item("episode-2", "第二集"),
        ],
        total_record_count: 2,
    });
    detail.selected_episode_id = Some("episode-1".to_string());

    assert_eq!(
        detail.hero_episode().map(|episode| episode.id.as_str()),
        Some("episode-1")
    );
    assert!(!detail.should_reveal_selected_episode());
}

fn media_source(id: &str, name: &str) -> MediaSource {
    MediaSource {
        id: Some(id.to_string()),
        item_id: None,
        name: Some(name.to_string()),
        path: None,
        source_type: None,
        container: None,
        size: None,
        bitrate: None,
        run_time_ticks: None,
        media_streams: None,
        default_subtitle_stream_index: None,
    }
}

#[test]
fn media_selection_uses_default_source_and_default_subtitle_stream_index() {
    let mut movie = user_item("movie-1", "电影");
    movie.item_type = Some("Movie".to_string());
    let mut detail = SeriesDetailModel::new_movie(&movie);
    let mut item = media_item("movie-1", "电影");
    item.item_type = Some("Movie".to_string());
    item.media_sources = Some(
        serde_json::from_value(serde_json::json!([
            {
                "Id": "source-1",
                "ItemId": "movie-1",
                "Type": "Grouping",
                "MediaStreams": [
                    { "Index": 0, "Type": "Video", "IsDefault": false }
                ]
            },
            {
                "Id": "source-2",
                "Type": "Default",
                "DefaultSubtitleStreamIndex": 6,
                "MediaStreams": [
                    { "Index": 0, "Type": "Video", "IsDefault": true },
                    { "Index": 4, "Type": "Subtitle", "DisplayTitle": "英文", "IsDefault": true },
                    { "Index": 6, "Type": "Subtitle", "DisplayTitle": "简体中文", "IsDefault": false }
                ]
            }
        ]))
        .unwrap(),
    );
    detail.item = Some(item);

    detail.sync_media_source_selection();

    assert_eq!(detail.selected_media_source_index(), Some(1));
    assert_eq!(
        detail.selected_subtitle_index(Default::default(), None),
        Some(1)
    );
    assert_eq!(
        detail.selected_subtitle_label(Default::default(), None),
        "简体中文"
    );
}

#[test]
fn resume_movie_restores_its_version_and_preserves_manual_selection() {
    let movie: ResumeItem = serde_json::from_value(serde_json::json!({
        "Id": "movie-1", "Name": "Movie", "Type": "Movie"
    }))
    .unwrap();
    let mut detail = SeriesDetailModel::from_resume_movie(&movie).unwrap();
    detail.resume_video_version = Some(VideoVersion {
        source_id: "resumed-source".into(),
        name: Some("Resumed version".into()),
        ..Default::default()
    });
    let mut item = media_item("movie-1", "Movie");
    item.media_sources = Some(
        serde_json::from_value(serde_json::json!([
            {"Id": "default-source", "Type": "Default"},
            {
                "Id": "resumed-source", "ItemId": "movie-1", "Name": "Resumed version",
                "MediaStreams": [
                    {"Type": "Subtitle", "Index": 3, "DisplayTitle": "简体中文", "IsDefault": true}
                ]
            }
        ]))
        .unwrap(),
    );
    detail.item = Some(item);

    detail.sync_media_source_selection();
    assert_eq!(detail.selected_media_source_index(), Some(1));
    assert_eq!(detail.selected_media_source_label(), "Resumed version");
    assert_eq!(
        detail.selected_subtitle_label(Default::default(), None),
        "简体中文"
    );

    detail.select_media_source(0);
    assert_eq!(detail.selected_media_source_index(), Some(0));
    detail
        .item
        .as_mut()
        .unwrap()
        .media_sources
        .as_mut()
        .unwrap()
        .swap(0, 1);
    detail.sync_media_source_selection();
    assert_eq!(detail.selected_media_source_index(), Some(1));
    assert_eq!(
        detail.selected_media_source().unwrap().id.as_deref(),
        Some("default-source")
    );
}

#[test]
fn remembered_video_overrides_an_automatic_selection_after_reload() {
    let movie: ResumeItem = serde_json::from_value(serde_json::json!({
        "Id": "movie-1", "Name": "Movie", "Type": "Movie"
    }))
    .unwrap();
    let mut detail = SeriesDetailModel::from_resume_movie(&movie).unwrap();
    detail.resume_video_version = Some(VideoVersion {
        source_id: "resumed-source".into(),
        name: None,
        ..Default::default()
    });
    let mut item = media_item("movie-1", "Movie");
    item.media_sources = Some(vec![media_source("default-source", "Default")]);
    detail.item = Some(item);
    detail.sync_media_source_selection();
    assert_eq!(detail.selected_media_source_index(), Some(0));

    detail.item.as_mut().unwrap().media_sources = Some(
        serde_json::from_value(serde_json::json!([
            {"Id": "default-source", "ItemId": "other-item", "Type": "Default"},
            {"Id": "resumed-source", "ItemId": "movie-1"}
        ]))
        .unwrap(),
    );
    detail.sync_media_source_selection();
    assert_eq!(detail.selected_media_source_index(), Some(1));
}

#[test]
fn resume_uses_playback_info_order_instead_of_item_id_or_default_type() {
    let movie: ResumeItem = serde_json::from_value(serde_json::json!({
        "Id": "movie-1", "Name": "Movie", "Type": "Movie"
    }))
    .unwrap();
    let mut detail = SeriesDetailModel::from_resume_movie(&movie).unwrap();
    let mut item = media_item("movie-1", "Movie");
    item.media_sources = Some(vec![media_source("metadata-source", "Metadata")]);
    detail.resume_media_sources = Some(
        serde_json::from_value(serde_json::json!([
            {"Id": "source-2160p", "ItemId": "other-item", "Name": "2160p", "Type": "Grouping"},
            {"Id": "source-1080p", "ItemId": "movie-1", "Type": "Default"}
        ]))
        .unwrap(),
    );
    detail.item = Some(item);
    detail.sync_media_source_selection();
    assert_eq!(detail.selected_media_source_index(), Some(0));
    assert_eq!(detail.selected_media_source_label(), "2160p");
}

#[test]
fn resume_movie_without_a_remembered_version_uses_the_first_source() {
    let movie: ResumeItem = serde_json::from_value(serde_json::json!({
        "Id": "movie-1", "Name": "Movie", "Type": "Movie"
    }))
    .unwrap();
    let mut detail = SeriesDetailModel::from_resume_movie(&movie).unwrap();
    let mut item = media_item("movie-1", "Movie");
    let mut default = media_source("source-default", "Default");
    default.source_type = Some("Default".to_string());
    item.media_sources = Some(vec![media_source("other-version", "Other"), default]);
    detail.item = Some(item);
    detail.sync_media_source_selection();
    assert_eq!(detail.selected_media_source_index(), Some(0));
}

#[test]
fn only_resume_episode_entry_restores_the_remembered_version() {
    let episode = resume_episode("episode-2", "Second", "series-1", 1);
    let user_episode: UserItem =
        serde_json::from_value(serde_json::to_value(&episode).unwrap()).unwrap();
    let resume_detail = SeriesDetailModel::from_resume_episode(&episode).unwrap();
    let user_detail = SeriesDetailModel::from_user_item(&user_episode).unwrap();
    for (mut detail, expected_index) in [(resume_detail, 1), (user_detail, 0)] {
        detail.resume_video_version = Some(VideoVersion {
            source_id: "mediasource_episode-2".into(),
            name: None,
            ..Default::default()
        });
        let mut item = media_item("episode-2", "Second");
        let mut default = media_source("default-source", "Default");
        default.source_type = Some("Default".to_string());
        let mut resumed_source = media_source("mediasource_episode-2", "Resumed");
        resumed_source.item_id = Some("episode-2".to_string());
        item.media_sources = Some(vec![default.clone(), resumed_source]);
        let mut next = media_item("episode-3", "Third");
        next.media_sources = Some(vec![
            default,
            media_source("mediasource_episode-3", "Other"),
        ]);
        detail.episodes = Some(MediaItems {
            items: vec![item, next],
            total_record_count: 2,
        });
        detail.choose_episode_from_loaded_episodes();
        assert_eq!(detail.selected_episode_id.as_deref(), Some("episode-2"));
        assert_eq!(detail.selected_media_source_index(), Some(expected_index));

        detail.preferred_episode_id = Some("episode-3".to_string());
        detail.apply_selected_episode(Some("episode-3".to_string()));
        assert_eq!(detail.selected_media_source_index(), Some(0));
    }
}

#[test]
fn resume_episode_version_selects_its_group_and_keeps_resume_position() {
    let mut episode = resume_episode("version-2", "Second", "series-1", 1);
    episode.user_data = Some(UserItemData {
        playback_position_ticks: Some(9_050_000_000),
        ..Default::default()
    });
    let mut detail = SeriesDetailModel::from_resume_episode(&episode).unwrap();
    let mut group = media_item("episode-2", "Second");
    group.media_sources = Some(
        serde_json::from_value(serde_json::json!([
            {"Id": "default-source", "Type": "Default"},
            {"Id": "resumed-source", "ItemId": "version-2"}
        ]))
        .unwrap(),
    );
    detail.resume_media_sources = Some(vec![group.media_sources.as_ref().unwrap()[1].clone()]);
    detail.episodes = Some(MediaItems {
        items: vec![media_item("episode-1", "First"), group],
        total_record_count: 2,
    });
    detail.choose_episode_from_loaded_episodes();
    assert_eq!(detail.selected_episode_id.as_deref(), Some("episode-2"));
    assert_eq!(detail.selected_media_source_index(), Some(0));
    assert!(detail.episode_selection_warning.is_none());
    assert!(detail.should_reveal_selected_episode());
    assert_eq!(detail.playback_position_seconds(), Some(905));
}

#[test]
fn media_selection_falls_back_to_default_stream_flags() {
    let mut movie = user_item("movie-1", "电影");
    movie.item_type = Some("Movie".to_string());
    let mut detail = SeriesDetailModel::new_movie(&movie);
    let mut item = media_item("movie-1", "电影");
    item.item_type = Some("Movie".to_string());
    item.media_sources = Some(
        serde_json::from_value(serde_json::json!([
            {
                "Id": "source-1",
                "MediaStreams": [
                    { "Index": 0, "Type": "Video", "IsDefault": false }
                ]
            },
            {
                "Id": "source-2",
                "MediaStreams": [
                    { "Index": 0, "Type": "Video", "IsDefault": true },
                    { "Index": 2, "Type": "Subtitle", "DisplayTitle": "英文强制", "IsDefault": false, "IsForced": true },
                    { "Index": 3, "Type": "Subtitle", "DisplayTitle": "简体中文", "IsDefault": true }
                ]
            }
        ]))
        .unwrap(),
    );
    detail.item = Some(item);

    detail.sync_media_source_selection();

    assert_eq!(detail.selected_media_source_index(), Some(1));
    assert_eq!(
        detail.selected_subtitle_index(Default::default(), None),
        Some(1)
    );
}

#[test]
fn media_selection_uses_forced_subtitle_as_the_last_fallback() {
    let mut movie = user_item("movie-1", "电影");
    movie.item_type = Some("Movie".to_string());
    let mut detail = SeriesDetailModel::new_movie(&movie);
    let mut item = media_item("movie-1", "电影");
    item.item_type = Some("Movie".to_string());
    item.media_sources = Some(
        serde_json::from_value(serde_json::json!([
            {
                "Id": "source-1",
                "Type": "Default",
                "MediaStreams": [
                    { "Index": 0, "Type": "Video", "IsDefault": true },
                    { "Index": 2, "Type": "Subtitle", "DisplayTitle": "普通字幕", "IsDefault": false, "IsForced": false },
                    { "Index": 3, "Type": "Subtitle", "DisplayTitle": "强制字幕", "IsDefault": false, "IsForced": true }
                ]
            }
        ]))
        .unwrap(),
    );
    detail.item = Some(item);

    detail.sync_media_source_selection();

    assert_eq!(
        detail.selected_subtitle_index(Default::default(), None),
        Some(1)
    );
    assert_eq!(
        detail.selected_subtitle_label(Default::default(), None),
        "强制字幕"
    );
}

#[test]
fn media_selection_falls_back_to_first_available_subtitle() {
    let mut movie = user_item("movie-1", "电影");
    movie.item_type = Some("Movie".to_string());
    let mut detail = SeriesDetailModel::new_movie(&movie);
    let mut item = media_item("movie-1", "电影");
    item.item_type = Some("Movie".to_string());
    item.media_sources = Some(
        serde_json::from_value(serde_json::json!([
            {
                "Id": "source-1",
                "Type": "Default",
                "MediaStreams": [
                    { "Index": 0, "Type": "Video", "IsDefault": true },
                    { "Index": 2, "Type": "Subtitle", "DisplayTitle": "第一字幕", "IsDefault": false, "IsForced": false },
                    { "Index": 3, "Type": "Subtitle", "DisplayTitle": "第二字幕", "IsDefault": false, "IsForced": false }
                ]
            }
        ]))
        .unwrap(),
    );
    detail.item = Some(item);

    assert_eq!(
        detail.selected_subtitle_index(Default::default(), None),
        Some(0)
    );
    assert_eq!(
        detail.selected_subtitle_label(Default::default(), None),
        "第一字幕"
    );

    detail.sync_media_source_selection();

    assert_eq!(
        detail.selected_subtitle_index(Default::default(), None),
        Some(0)
    );
}

#[test]
fn negative_default_subtitle_index_continues_to_available_stream_fallbacks() {
    let mut movie = user_item("movie-1", "电影");
    movie.item_type = Some("Movie".to_string());
    let mut detail = SeriesDetailModel::new_movie(&movie);
    let mut item = media_item("movie-1", "电影");
    item.item_type = Some("Movie".to_string());
    item.media_sources = Some(
        serde_json::from_value(serde_json::json!([
            {
                "Id": "source-1",
                "Type": "Default",
                "DefaultSubtitleStreamIndex": -1,
                "MediaStreams": [
                    { "Index": 0, "Type": "Video", "IsDefault": true },
                    { "Index": 2, "Type": "Subtitle", "DisplayTitle": "普通字幕", "IsDefault": false, "IsForced": false },
                    { "Index": 3, "Type": "Subtitle", "DisplayTitle": "简体中文", "IsDefault": true, "IsForced": false }
                ]
            }
        ]))
        .unwrap(),
    );
    detail.item = Some(item);

    detail.sync_media_source_selection();

    assert_eq!(detail.selected_media_source_index(), Some(0));
    assert_eq!(
        detail.selected_subtitle_index(Default::default(), None),
        Some(1)
    );
    assert_eq!(
        detail.selected_subtitle_label(Default::default(), None),
        "简体中文"
    );
}

#[test]
fn movie_detail_uses_item_as_playback_source() {
    let mut movie = user_item("movie-1", "电影");
    movie.item_type = Some("Movie".to_string());
    let mut detail = SeriesDetailModel::new_movie(&movie);
    let mut item = media_item("movie-1", "电影");
    item.item_type = Some("Movie".to_string());
    item.media_sources = Some(vec![media_source("source-1", "4K HDR")]);
    item.user_data = Some(UserItemData {
        unplayed_item_count: None,
        played_percentage: Some(25.0),
        playback_position_ticks: Some(10_800_000_000),
        is_favorite: false,
        played: false,
    });
    detail.item = Some(item);

    detail.sync_media_source_selection();

    assert!(detail.is_movie());
    assert_eq!(detail.hero_line(), None);
    assert_eq!(detail.selected_media_source_label(), "4K HDR");
    assert_eq!(
        detail.selected_playback_item().map(|item| item.id.as_str()),
        Some("movie-1")
    );
    assert_eq!(detail.playback_position_seconds(), Some(1_080));
}

#[test]
fn from_user_item_accepts_series_movie_and_routable_episode() {
    let series = user_item("series-1", "剧集");
    let mut movie = user_item("movie-1", "电影");
    movie.item_type = Some("Movie".to_string());
    let mut folder = user_item("folder-1", "合集");
    folder.item_type = Some("Folder".to_string());
    let episode: UserItem = serde_json::from_value(serde_json::json!({
        "Id": "episode-1",
        "Name": "第一集",
        "Type": "Episode",
        "SeriesId": "series-1",
        "SeriesName": "剧集",
        "ParentIndexNumber": 1,
        "IndexNumber": 1
    }))
    .unwrap();

    assert!(SeriesDetailModel::from_user_item(&series).is_some_and(|detail| detail.is_series()));
    assert!(SeriesDetailModel::from_user_item(&movie).is_some_and(|detail| detail.is_movie()));
    assert!(
        SeriesDetailModel::from_user_item(&episode).is_some_and(|detail| {
            detail.is_series() && detail.preferred_episode_id.as_deref() == Some("episode-1")
        })
    );
    assert!(SeriesDetailModel::from_user_item(&folder).is_none());
}

#[test]
fn episode_parent_id_selects_the_exact_season_without_an_index_number() {
    let episode: UserItem = serde_json::from_value(serde_json::json!({
        "Id": "episode-2",
        "Name": "第二集",
        "Type": "Episode",
        "SeriesId": "series-1",
        "SeriesName": "剧集",
        "ParentId": "season-2",
        "IndexNumber": 2
    }))
    .unwrap();
    let mut detail = SeriesDetailModel::from_user_item(&episode).unwrap();
    let mut season_one = media_item("season-1", "第一季");
    season_one.index_number = Some(1);
    let mut season_two = media_item("season-2", "第二季");
    season_two.index_number = Some(2);
    detail.seasons = Some(MediaItems {
        items: vec![season_one, season_two],
        total_record_count: 2,
    });

    detail.choose_season_if_needed();

    assert_eq!(detail.selected_season_id.as_deref(), Some("season-2"));
}

#[test]
fn resume_episode_parent_id_selects_the_exact_season_without_an_index_number() {
    let mut episode = resume_episode("episode-2", "第二集", "series-1", 2);
    episode.parent_id = Some("season-2".to_string());
    episode.parent_index_number = None;
    let mut detail = SeriesDetailModel::from_resume_episode(&episode).unwrap();
    let mut season_one = media_item("season-1", "第一季");
    season_one.index_number = Some(1);
    let mut season_two = media_item("season-2", "第二季");
    season_two.index_number = Some(2);
    detail.seasons = Some(MediaItems {
        items: vec![season_one, season_two],
        total_record_count: 2,
    });

    detail.choose_season_if_needed();

    assert_eq!(detail.selected_season_id.as_deref(), Some("season-2"));
}

#[test]
fn deleted_preferred_episode_falls_back_and_sets_warning() {
    let episode = resume_episode("deleted-episode", "已删除", "series-1", 1);
    let mut detail = SeriesDetailModel::from_resume_episode(&episode).unwrap();
    detail.episodes = Some(MediaItems {
        items: vec![media_item("episode-1", "第一集")],
        total_record_count: 1,
    });

    detail.choose_episode_from_loaded_episodes();

    assert_eq!(detail.selected_episode_id.as_deref(), Some("episode-1"));
    assert!(detail.episode_selection_warning.is_some());
}

#[test]
fn resume_episode_entry_defers_media_loading_and_selects_current_episode() {
    let mut episode = resume_episode("episode-2", "第二集", "series-1", 2);
    episode.user_data = Some(UserItemData {
        unplayed_item_count: None,
        played_percentage: Some(50.0),
        playback_position_ticks: Some(9_050_000_000),
        is_favorite: false,
        played: false,
    });

    let mut detail = SeriesDetailModel::from_resume_episode(&episode).expect("valid episode");
    let mut season_one = media_item("season-1", "第一季");
    season_one.index_number = Some(1);
    let mut season_two = media_item("season-2", "第二季");
    season_two.index_number = Some(2);
    detail.seasons = Some(MediaItems {
        items: vec![season_one, season_two],
        total_record_count: 2,
    });
    detail.choose_season_if_needed();
    detail.episodes = Some(MediaItems {
        items: vec![
            media_item("episode-1", "第一集"),
            media_item("episode-2", "第二集"),
        ],
        total_record_count: 2,
    });
    detail.choose_episode_from_loaded_episodes();

    assert!(detail.opened_from_resume());
    assert!(!detail.should_load_next_up());
    assert_eq!(detail.effects.item, LoadState::Idle);
    assert!(detail.item.is_none());
    assert_eq!(detail.selected_season_id.as_deref(), Some("season-2"));
    assert_eq!(detail.selected_episode_id.as_deref(), Some("episode-2"));
    assert_eq!(detail.selected_episode_index(), Some(1));
    assert!(detail.should_reveal_selected_episode());
    assert_eq!(detail.playback_position_seconds(), Some(905));
}

#[test]
fn next_up_position_drives_user_view_resume_minutes() {
    let mut detail = SeriesDetailModel::new_series(&user_item("series-1", "示例剧集"));
    let mut next_up = media_item("episode-2", "第二集");
    next_up.user_data = Some(UserItemData {
        unplayed_item_count: None,
        played_percentage: None,
        playback_position_ticks: Some(12_000_000_000),
        is_favorite: false,
        played: false,
    });
    detail.next_up = Some(MediaItems {
        items: vec![next_up],
        total_record_count: 1,
    });
    detail.episodes = Some(MediaItems {
        items: vec![media_item("episode-2", "第二集")],
        total_record_count: 1,
    });
    detail.selected_episode_id = Some("episode-2".to_string());

    assert!(detail.should_load_next_up());
    assert_eq!(detail.playback_position_seconds(), Some(1_200));
}

#[test]
fn playback_update_selects_replacement_episode_and_updates_closed_item() {
    let mut detail = SeriesDetailModel::new_series(&user_item("series-1", "Series"));
    detail.selected_season_id = Some("season-1".to_string());
    detail.episodes = Some(MediaItems {
        items: vec![
            media_item("episode-1", "First"),
            media_item("episode-2", "Second"),
        ],
        total_record_count: 2,
    });
    detail.selected_episode_id = Some("episode-1".to_string());
    let update = crate::player::PlaybackStateUpdate {
        item_id: "episode-1-2160p".to_string(),
        list_item_id: "episode-1".to_string(),
        media_source_id: "source-2160p".to_string(),
        media_source_name: None,
        series_id: Some("series-1".to_string()),
        season_id: Some("season-1".to_string()),
        position_ticks: 250,
        run_time_ticks: Some(1_000),
        ended: false,
        failed: false,
        selected_item_id: Some("episode-2".to_string()),
        stop_completion: None,
    };
    let user_data = UserItemData {
        playback_position_ticks: Some(250),
        played_percentage: Some(25.0),
        ..UserItemData::default()
    };

    detail.apply_playback_update(&update, &user_data);

    assert_eq!(detail.selected_episode_id.as_deref(), Some("episode-2"));
    assert_eq!(
        detail
            .episodes
            .as_ref()
            .and_then(|episodes| episodes.items.first())
            .and_then(|episode| episode.playback_position_ticks()),
        Some(250)
    );
}
