//! Shared in-memory playback page fixtures; no native playback devices.
use super::episodes::PlaybackEpisodeListState;
use super::*;
use gpui::{Entity, TestAppContext, VisualTestContext, size};
use serde_json::json;

pub(super) fn episode(index: usize) -> PlaybackQueueItem {
    PlaybackQueueItem {
        item_id: format!("episode-{index}"),
        title: format!("Series S1E{}", index + 1).into(),
        episode_label: format!("E{}: Episode title", index + 1).into(),
        overview: Some(
            "An episode overview that is long enough to wrap across multiple lines in the list."
                .into(),
        ),
        primary_image_tag: None,
        series_id: Some("series".into()),
        season_id: Some("season".into()),
        premiere_date: Some("1998-04-03T00:00:00.0000000Z".into()),
        run_time_ticks: Some(18_000_000_000),
        playback_position_ticks: Some(100_000_000),
        media_sources: vec![
            serde_json::from_value(json!({
                "Id": format!("source-{index}"), "Size": 1_320_702_444_u64
            }))
            .unwrap(),
        ],
    }
}

pub(in crate::player::page) fn playback_window(
    cx: &mut TestAppContext,
) -> (Entity<PlaybackPage>, &mut VisualTestContext) {
    cx.update(theme::init);
    let (page, cx) = cx.add_window_view(|_, cx| PlaybackPage::test_fixture(cx));
    cx.simulate_resize(size(px(1100.0), px(800.0)));
    cx.run_until_parked();
    (page, cx)
}

impl PlaybackPage {
    /// In-memory page for interaction tests; no FFmpeg, Vulkan or audio device.
    pub(crate) fn test_fixture(cx: &mut gpui::Context<Self>) -> Self {
        let emby = playback_context();
        let ports = crate::player::adapter::playback_ports(&emby);
        Self::fixture_with_context(emby, ports, cx)
    }

    pub(in crate::player::page) fn test_fixture_with_ports(
        ports: crate::player::PlaybackPorts,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        Self::fixture_with_context(playback_context(), ports, cx)
    }

    fn fixture_with_context(
        emby: EmbyPlaybackContext,
        ports: crate::player::PlaybackPorts,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let (report_effects, queue_effects) = lifecycle::bind_ports(&emby, ports, cx);
        // Exercise the real page and event flow without starting FFmpeg or Vulkan.
        PlaybackPage::register_image_cleanup(cx);
        let mut presentation = PlaybackPresentationState::new(
            cx.focus_handle(),
            emby.server.workspace_identity(),
            PlaybackEpisodeListState::new(&emby),
        );
        presentation.fullscreen.controls_visible = true;
        presentation.fullscreen.cursor_visible = true;
        PlaybackPage {
            presentation,
            title: "Series S1E1".into(),
            video: PlaybackBackendAdapter::empty(),
            backend_poll: Default::default(),
            power: power::PlaybackPower::for_tests(),
            session: crate::player::session::PlaybackSessionController::new(
                PlaybackTimelineState {
                    loaded: true,
                    duration: Some(1800.0),
                    position: Some(45.0),
                    user_paused: false,
                    paused: false,
                    ..Default::default()
                },
                PlaybackSourceState {
                    source_protocol: None,
                    source_url: "episode.mkv".to_string(),
                    content_length: None,
                    playback_file_info: None,
                    playback_info: None,
                    playback_audio_info: None,
                    track_preference_key: crate::player::PlaybackTrackPreferenceKey {
                        item_id: "episode-0".into(),
                        media_source_id: "source-0".into(),
                    },
                    tracks: PlaybackTrackState::new(
                        Vec::new(),
                        Vec::new(),
                        PlaybackTrackSelection::default(),
                    ),
                    remember_subtitle_on_start: false,
                },
                PlaybackQueue::new((0..12).map(episode).collect(), 0),
                emby.server.workspace_identity(),
                PlaybackVolumeSettings::default(),
                None,
            ),
            report_effects,
            queue_effects,
            emby,
            gamepad: gamepad::PlaybackGamepad::disabled(),
        }
    }
}

fn playback_context() -> EmbyPlaybackContext {
    EmbyPlaybackContext {
        client: crate::emby::EmbyClient::new("episode-list-test".into()).unwrap(),
        server: serde_json::from_value(json!({
            "id": "episode-list-test",
            "endpoint": {"protocol": "Http", "address": "", "port": 80, "path": "/emby"},
            "username": "test", "password": "", "user_id": "user",
            "access_token": "test-token", "added_at_unix": 0
        }))
        .unwrap(),
        item_id: "episode-0".into(),
        media_source_id: "source-0".into(),
        play_session_id: None,
        run_time_ticks: Some(18_000_000_000),
    }
}
