use std::{cell::RefCell, path::Path, rc::Rc, time::Duration};

use gpui::{Entity, Modifiers, TestAppContext, VisualTestContext, px, size};

use crate::player::{PlaybackTrackKind, PlaybackTrackPreferences, SavedTrackChoice, TrackLanguage};

use super::*;

fn server() -> CachedServer {
    serde_json::from_value(serde_json::json!({
        "id": "detail-subtitle-test", "user_id": "user",
        "endpoint": {"protocol": "Https", "address": "example.com", "port": 443, "path": ""},
        "username": "test", "password": "", "added_at_unix": 0
    }))
    .unwrap()
}

fn detail() -> SeriesDetailState {
    let item: UserItem = serde_json::from_value(serde_json::json!({
        "Id": "item-1", "Name": "Video", "Type": "Movie"
    }))
    .unwrap();
    let mut detail = SeriesDetailState::from_user_item(&item).unwrap();
    let source = serde_json::json!({
        "Id": "source-1", "DefaultSubtitleStreamIndex": 10,
        "MediaStreams": [
            {"Index": 9, "Type": "Subtitle", "Codec": "ass", "Language": "chi", "DisplayTitle": "Chinese Simplified (默认 ASS)", "IsDefault": true},
            {"Index": 10, "Type": "Subtitle", "Codec": "ass", "Language": "chi", "DisplayTitle": "Chinese Simplified (ASS)"}
        ]
    });
    let mut other = source.clone();
    other["Id"] = "source-2".into();
    detail.item = Some(
        serde_json::from_value(serde_json::json!({
            "Id": "item-1", "Name": "Video", "Type": "Movie",
            "MediaSources": [source, other]
        }))
        .unwrap(),
    );
    detail.sync_media_source_selection();
    detail
}

fn home(cx: &mut TestAppContext, path: &Path) -> Entity<HomeContent> {
    let page = cx.new(|cx| {
        let mut page = HomeContent::new(
            server(),
            crate::emby::EmbyClient::new("test".into()).unwrap(),
            cx,
        );
        page.snapshot_save_path = Some(path.to_path_buf());
        page.series_detail = Some(detail());
        page
    });
    cx.run_until_parked();
    page
}

fn selection(page: &HomeContent, cx: &gpui::App) -> SelectedPlayback {
    let detail = page.series_detail.as_ref().unwrap();
    selected_playback(
        detail,
        &page.current_server,
        Default::default(),
        &detail.selected_track_choices(&page.current_server, cx),
    )
    .unwrap()
}

#[gpui::test]
fn unplayed_detail_subtitles_are_temporary_and_isolated_by_version(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("snapshot.json");
    let page = home(cx, &path);
    page.update(cx, |page, cx| {
        assert_eq!(
            selection(page, cx).selected_tracks.subtitle_stream_index,
            Some(10)
        );
        assert!(!selection(page, cx).remember_subtitle_on_start);
        page.select_series_subtitle(Some(0), cx);
        assert_eq!(
            selection(page, cx).selected_tracks.subtitle_stream_index,
            Some(9)
        );
        assert!(selection(page, cx).remember_subtitle_on_start);
        let detail = page.series_detail.as_ref().unwrap();
        let choices = detail.selected_track_choices(&page.current_server, cx);
        assert_eq!(
            detail.selected_subtitle_label(TrackLanguage::Default, choices.subtitle.as_ref()),
            "Chinese Simplified (默认 ASS)"
        );
        assert!(
            PlaybackTrackPreferences::get(
                &page.current_server,
                &detail.track_preference_key().unwrap(),
                cx
            )
            .subtitle
            .is_none()
        );

        page.select_series_media_source(1, cx);
        assert_eq!(
            selection(page, cx).selected_tracks.subtitle_stream_index,
            Some(10)
        );
        assert!(!selection(page, cx).remember_subtitle_on_start);
        page.select_series_subtitle(Some(1), cx);
        page.select_series_media_source(0, cx);
        assert_eq!(
            selection(page, cx).selected_tracks.subtitle_stream_index,
            Some(9)
        );
        assert!(!page.snapshot_save_pending);
        assert!(page.home_snapshot().played_video_versions.is_empty());
    });
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_millis(450));
    cx.run_until_parked();
    assert!(!path.exists());
    cx.update(|_| drop(page));
    cx.run_until_parked();
    assert!(!path.exists());
    let reopened = home(cx, &path);
    reopened.read_with(cx, |page, cx| {
        assert_eq!(
            selection(page, cx).selected_tracks.subtitle_stream_index,
            Some(10)
        );
        assert!(!selection(page, cx).remember_subtitle_on_start);
    });
}

#[gpui::test]
fn unplayed_detail_subtitles_do_not_overwrite_saved_off_on_close(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("snapshot.json");
    let page = home(cx, &path);
    let key = page.read_with(cx, |page, _| {
        page.series_detail
            .as_ref()
            .unwrap()
            .track_preference_key()
            .unwrap()
    });
    cx.update(|cx| {
        PlaybackTrackPreferences::remember(
            &server(),
            std::slice::from_ref(&key),
            PlaybackTrackKind::Subtitle,
            None,
            cx,
        );
    });
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_millis(450));
    cx.run_until_parked();
    let saved = std::fs::read(&path).unwrap();
    page.update(cx, |page, cx| {
        assert_eq!(
            selection(page, cx).selected_tracks.subtitle_stream_index,
            None
        );
        page.select_series_subtitle(Some(0), cx);
        assert_eq!(
            selection(page, cx).selected_tracks.subtitle_stream_index,
            Some(9)
        );
        assert_eq!(
            PlaybackTrackPreferences::get(&server(), &key, cx).subtitle,
            Some(SavedTrackChoice::Off)
        );
        assert!(!page.snapshot_save_pending);
    });
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_millis(450));
    cx.run_until_parked();
    cx.update(|_| drop(page));
    cx.run_until_parked();
    assert_eq!(std::fs::read(&path).unwrap(), saved);

    cx.update(|cx| cx.set_global(PlaybackTrackPreferences::default()));
    let reopened = home(cx, &path);
    reopened.update(cx, |page, cx| {
        page.hydrate_home_snapshot(serde_json::from_slice(&saved).unwrap(), cx);
        assert_eq!(
            selection(page, cx).selected_tracks.subtitle_stream_index,
            None
        );
        assert!(!selection(page, cx).remember_subtitle_on_start);
    });
}

#[gpui::test]
fn detail_subtitle_draft_moves_to_playback_request_without_persisting(cx: &mut TestAppContext) {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("snapshot.json");
    let page = home(cx, &path);
    let opened = Rc::new(RefCell::new(None));
    let _subscription = cx.update(|cx| {
        let opened = opened.clone();
        cx.subscribe(&page, move |_, event: &HomeContentEvent, _| {
            if let HomeContentEvent::OpenPlayback(request) = event {
                opened.replace(Some(request.clone()));
            }
        })
    });
    page.update(cx, |page, cx| {
        page.select_series_subtitle(Some(0), cx);
        let selected = selection(page, cx);
        page.finish_play_selected_media(
            page.request_identity(),
            page.detail_generation,
            selected,
            Err(anyhow::anyhow!("unavailable")),
            cx,
        );
        assert_eq!(
            selection(page, cx).selected_tracks.subtitle_stream_index,
            Some(9)
        );
        assert!(selection(page, cx).remember_subtitle_on_start);
        assert!(page.home_snapshot().played_video_versions.is_empty());
        let selected = selection(page, cx);
        page.finish_play_selected_media(
            page.request_identity(),
            page.detail_generation,
            selected,
            Ok(ResolvedPlayback {
                item_id: "resolved-item".into(),
                media_source_id: "resolved-source".into(),
                url: "https://example.com/video.mkv".into(),
                http_headers: Vec::new(),
                content_length: None,
                play_session_id: None,
            }),
            cx,
        );
        assert!(
            page.series_detail
                .as_ref()
                .unwrap()
                .pending_subtitle_choice()
                .is_none()
        );
        assert!(page.home_snapshot().played_video_versions.is_empty());
    });
    cx.run_until_parked();
    let request = opened.borrow().clone().unwrap();
    assert!(request.remember_subtitle_on_start);
    assert_eq!(request.selected_tracks.subtitle_stream_index, Some(9));
    assert_eq!(request.track_preference_key.item_id, "item-1");
    assert_eq!(request.track_preference_key.media_source_id, "source-1");
    cx.update(|cx| {
        assert!(
            PlaybackTrackPreferences::get(&server(), &request.track_preference_key, cx)
                .subtitle
                .is_none()
        );
    });
    cx.update(|_| drop(page));
    cx.run_until_parked();
    assert!(!path.exists());
}

fn detail_menu_window(cx: &mut TestAppContext) -> (Entity<HomeContent>, &mut VisualTestContext) {
    cx.update(crate::theme::init);
    let (page, cx) = cx.add_window_view(|_, cx| {
        let mut page = HomeContent::new(
            server(),
            crate::emby::EmbyClient::new("test".into()).unwrap(),
            cx,
        );
        let mut detail = detail();
        detail
            .item
            .as_mut()
            .unwrap()
            .media_sources
            .as_mut()
            .unwrap()[0]
            .media_streams
            .as_mut()
            .unwrap()[0]
            .title = Some("简体中文 · 双语特效字幕".into());
        page.navigation.push_detail("item-1".into(), None);
        page.series_detail = Some(detail);
        page
    });
    cx.simulate_resize(size(px(1100.0), px(900.0)));
    cx.run_until_parked();
    (page, cx)
}

fn click(cx: &mut VisualTestContext, selector: &'static str) {
    let bounds = cx.debug_bounds(selector).expect(selector);
    cx.simulate_click(bounds.center(), Modifiers::default());
    cx.run_until_parked();
}

#[gpui::test]
fn subtitle_menu_shows_language_with_or_without_title_and_applies_off_as_a_draft(
    cx: &mut TestAppContext,
) {
    let (page, cx) = detail_menu_window(cx);
    for theme in crate::theme::ColorTheme::ALL {
        cx.update(|_, cx| crate::theme::set(theme, cx));
        click(cx, "series-detail-subtitle-select");
        for (row, label, title) in [
            (
                "series-detail-subtitle-off-option",
                "series-detail-subtitle-off-option-label",
                "series-detail-subtitle-off-option-title",
            ),
            (
                "series-detail-subtitle-option-0",
                "series-detail-subtitle-option-0-label",
                "series-detail-subtitle-option-0-title",
            ),
            (
                "series-detail-subtitle-option-1",
                "series-detail-subtitle-option-1-label",
                "series-detail-subtitle-option-1-title",
            ),
        ] {
            let row = cx.debug_bounds(row).unwrap();
            let label = cx.debug_bounds(label).unwrap();
            let title = cx.debug_bounds(title).unwrap();
            assert!(label.bottom() <= title.top());
            assert!(title.size.height < label.size.height);
            assert!(label.top() >= row.top());
            assert!(title.bottom() <= row.bottom());
            assert!(label.right() <= row.right());
            assert!(title.right() <= row.right());
        }
        click(cx, "series-detail-subtitle-off-option");
        assert!(cx.debug_bounds("series-detail-subtitle-menu").is_none());
        page.read_with(cx, |page, cx| {
            let detail = page.series_detail.as_ref().unwrap();
            let choices = detail.selected_track_choices(&page.current_server, cx);
            assert_eq!(choices.subtitle, Some(SavedTrackChoice::Off));
            assert_eq!(
                detail.selected_subtitle_label(TrackLanguage::Default, choices.subtitle.as_ref()),
                "Off"
            );
            let selected = selection(page, cx);
            assert!(selected.remember_subtitle_on_start);
            assert_eq!(selected.selected_tracks.subtitle_stream_index, None);
            assert_eq!(selected.selected_tracks.subtitle_external_url, None);
            assert_eq!(selected.selected_tracks.subtitle_codec, None);
            assert!(page.home_snapshot().played_video_versions.is_empty());
            assert!(!page.snapshot_save_pending);
        });
        click(cx, "series-detail-subtitle-select");
        click(cx, "series-detail-subtitle-option-0");
        page.read_with(cx, |page, cx| {
            assert_eq!(
                selection(page, cx).selected_tracks.subtitle_stream_index,
                Some(9)
            );
        });
    }
}

#[gpui::test]
fn subtitle_menu_reveals_selected_track_after_off_and_stays_closed_without_subtitles(
    cx: &mut TestAppContext,
) {
    let (page, cx) = detail_menu_window(cx);
    page.update(cx, |page, cx| {
        let detail = page.series_detail.as_mut().unwrap();
        let source = &mut detail
            .item
            .as_mut()
            .unwrap()
            .media_sources
            .as_mut()
            .unwrap()[0];
        let template = source.media_streams.as_ref().unwrap()[0].clone();
        source.media_streams = Some(
            (0..8)
                .map(|index| {
                    let mut stream = template.clone();
                    stream.index = Some(index + 9);
                    stream.title = Some(
                        "A long subtitle title that should be truncated without widening the menu"
                            .into(),
                    );
                    stream
                })
                .collect(),
        );
        page.select_series_subtitle(Some(7), cx);
    });
    cx.run_until_parked();
    click(cx, "series-detail-subtitle-select");
    let menu = cx.debug_bounds("series-detail-subtitle-menu").unwrap();
    let selected = cx.debug_bounds("series-detail-subtitle-option-7").unwrap();
    assert!(menu.size.height < px(300.0));
    assert!(selected.top() >= menu.top());
    assert!(
        selected.bottom() <= menu.bottom(),
        "selected: {selected:?}, menu: {menu:?}, scroll: {:?}",
        page.read_with(cx, |page, _| {
            let handle = &page.series_detail.as_ref().unwrap().subtitle_scroll_handle;
            (handle.offset(), handle.max_offset(), handle.bounds())
        })
    );
    let title = cx
        .debug_bounds("series-detail-subtitle-option-7-title")
        .unwrap();
    assert!(title.right() <= menu.right());
    click(cx, "series-detail-subtitle-option-7");

    page.update(cx, |page, cx| {
        let detail = page.series_detail.as_mut().unwrap();
        detail
            .item
            .as_mut()
            .unwrap()
            .media_sources
            .as_mut()
            .unwrap()[0]
            .media_streams = Some(Vec::new());
        detail.pending_subtitle_choices.clear();
        detail.sync_media_source_selection();
        cx.notify();
    });
    cx.run_until_parked();
    click(cx, "series-detail-subtitle-select");
    assert!(cx.debug_bounds("series-detail-subtitle-menu").is_none());
    assert!(cx.debug_bounds("series-detail-subtitle-option-0").is_none());
    assert!(
        cx.debug_bounds("series-detail-subtitle-off-option")
            .is_none()
    );
    page.read_with(cx, |page, cx| {
        let detail = page.series_detail.as_ref().unwrap();
        assert!(detail.open_select.is_none());
        assert!(detail.pending_subtitle_choice().is_none());
        for preference in [None, Some(&SavedTrackChoice::Off)] {
            assert_eq!(
                detail.selected_subtitle_label(TrackLanguage::Default, preference),
                "无字幕"
            );
        }
        assert_eq!(
            selection(page, cx).selected_tracks.subtitle_stream_index,
            None
        );
        assert!(!selection(page, cx).remember_subtitle_on_start);
    });
}

#[gpui::test]
fn video_menu_shows_metadata_and_reveals_the_selected_version(cx: &mut TestAppContext) {
    let (page, cx) = detail_menu_window(cx);
    page.update(cx, |page, cx| {
        page.series_detail
            .as_mut()
            .unwrap()
            .item
            .as_mut()
            .unwrap()
            .media_sources = Some(
            (0..8)
                .map(|index| {
                    serde_json::from_value(serde_json::json!({
                        "Id": format!("source-{index}"),
                        "Name": format!("星际牛仔.1998.S01E01.1080p.BluRay.Remux.SDR.{index}.mkv"),
                        "Size": 13_249_974_108_u64,
                        "Bitrate": 38_543_210,
                        "MediaStreams": [{"Type": "Video", "DisplayTitle": "1080p H264"}]
                    }))
                    .unwrap()
                })
                .collect(),
        );
        page.select_series_media_source(7, cx);
    });
    cx.run_until_parked();
    click(cx, "series-detail-video-select");

    let menu = cx.debug_bounds("series-detail-video-menu").unwrap();
    let row = cx.debug_bounds("series-detail-video-option-7").unwrap();
    let label = cx
        .debug_bounds("series-detail-video-option-7-label")
        .unwrap();
    let subtitle = cx
        .debug_bounds("series-detail-video-option-7-title")
        .unwrap();
    assert!(menu.size.height < px(300.0));
    assert!(row.top() >= menu.top());
    assert!(row.bottom() <= menu.bottom());
    assert!(label.bottom() <= subtitle.top());
    assert!(subtitle.size.height < label.size.height);
    assert!(label.top() >= row.top());
    assert!(subtitle.bottom() <= row.bottom());
    assert!(label.right() <= row.right());
    assert!(subtitle.right() <= row.right());

    click(cx, "series-detail-video-option-6");
    assert!(cx.debug_bounds("series-detail-video-menu").is_none());
    page.read_with(cx, |page, cx| {
        assert_eq!(selection(page, cx).media_source_id, "source-6");
        assert!(page.home_snapshot().played_video_versions.is_empty());
        assert!(!page.snapshot_save_pending);
    });
}
