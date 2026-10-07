use tiny_playback::BackendEvent;

use super::*;
#[cfg(test)]
use crate::player::model::timeline::{
    apply_cache_buffering_to_timeline, apply_paused_for_cache_to_timeline,
    apply_playback_restart_to_timeline,
};
#[cfg(test)]
use crate::player::session::cache_state_needs_poll;
use crate::player::session::{BackendAction, BackendContext};

const PAUSED_BACKEND_POLL_INTERVAL: Duration = Duration::from_millis(250);

impl PlaybackPage {
    pub(super) fn poll_backend(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for event in self.poll_backend_events() {
            self.apply_backend_event(event, window, cx);
        }
        self.poll_video_presenter(window, cx);
        self.schedule_paused_backend_poll(cx);
        self.sync_playback_power(window, cx);
    }

    fn poll_backend_events(&mut self) -> Vec<BackendEvent> {
        self.video.poll_events()
    }

    fn apply_backend_event(
        &mut self,
        event: BackendEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let transition = self.session.reduce_backend(
            event.kind,
            BackendContext {
                item_id: &self.emby.item_id,
                media_source_id: &self.emby.media_source_id,
                play_session_id: self.emby.play_session_id.as_deref(),
                run_time_ticks: self.emby.run_time_ticks,
                has_frame: self.presentation.frame.current.is_some(),
            },
        );
        let mut closed_update = None;
        for reporting in transition.reporting {
            if let Some(update) = self.execute_reporting_transition(reporting, cx) {
                closed_update = Some(update);
            }
        }
        match transition.action {
            BackendAction::None => {}
            BackendAction::Diagnostic(diagnostic) => {
                tracing::debug!(code = diagnostic.code, message = %diagnostic.message, "playback backend diagnostic event");
            }
            BackendAction::Restart => {
                self.presentation.timeline_presentation.cache_status_open = false;
            }
            BackendAction::Failed => {
                self.backend_poll.cancel();
                self.cancel_queue_switch();
                self.reset_after_backend_failure(window, cx);
            }
            BackendAction::Ended { auto_next } => {
                self.backend_poll.cancel();
                self.finish_playback(window, cx);
                if auto_next {
                    self.switch_to_next_episode_after_end(window, cx);
                } else {
                    cx.emit(PlaybackEvent::Update {
                        update: closed_update.expect("ended transition closes reporting"),
                    });
                }
            }
            BackendAction::TracksChanged => {
                self.presentation.track_select_open = None;
            }
            BackendAction::Subtitle(cue) => {
                for image in self.presentation.subtitle.images.update(cue.as_ref()) {
                    defer_drop_frame(image, window);
                }
                self.presentation.subtitle.active = cue;
            }
            BackendAction::VideoSize(size) => {
                if self.presentation.frame.source_size != size {
                    self.presentation.frame.source_size = size;
                    self.clear_visible_frame(window, cx);
                }
            }
        }
        self.release_power_if_idle();
    }

    fn schedule_paused_backend_poll(&mut self, cx: &mut Context<Self>) {
        let has_backend = self.video.has_backend();
        let has_error = self.session.controls_view().error.is_some();
        if !self.session.should_poll(has_backend, has_error) {
            self.session.cancel_poll();
            self.backend_poll.cancel();
            return;
        }
        let Some(token) = self.session.begin_poll(has_backend, has_error) else {
            return;
        };
        self.backend_poll.replace(cx.spawn(async move |page, cx| {
            cx.background_executor()
                .timer(PAUSED_BACKEND_POLL_INTERVAL)
                .await;
            page.update(cx, |page, cx| {
                if page.session.complete_poll(
                    &token,
                    &page.emby.server.workspace_identity(),
                    page.video.has_backend(),
                    page.session.controls_view().error.is_some(),
                ) {
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    fn finish_playback(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.presentation.timeline_presentation.cache_status_open = false;
        self.presentation.track_select_open = None;
        defer_drop_subtitle(&mut self.presentation.subtitle, window);
        cx.notify();
    }

    fn reset_after_backend_failure(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.presentation.frame.source_size = None;
        self.presentation.timeline_presentation.cache_status_open = false;
        defer_drop_subtitle(&mut self.presentation.subtitle, window);
        self.clear_visible_frame(window, cx);
    }

    fn poll_video_presenter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.video.prewarm();

        let render_size = self
            .presentation
            .frame
            .viewport_bounds
            .zip(self.presentation.frame.source_size)
            .and_then(|(viewport_bounds, source_size)| {
                render_output_size(viewport_bounds, source_size)
            });
        if should_render_frame(
            self.video.has_presenter(),
            self.session.timeline().loaded,
            self.session.controls_view().error.is_some(),
            self.presentation.frame.source_size.is_some(),
            render_size.is_some(),
        ) {
            let size = render_size.expect("render size checked above");
            let render_result = self.video.render(size);
            let presenter_snapshot = self
                .video
                .presenter_snapshot()
                .expect("video presenter checked above");
            if let Some(blocked_on) = presenter_snapshot.blocked_on {
                tracing::trace!(
                    blocked_on,
                    queued = presenter_snapshot.queued,
                    queue_capacity = presenter_snapshot.queue_capacity,
                    rendering = presenter_snapshot.rendering,
                    ready = presenter_snapshot.ready,
                    pending_render_requests = presenter_snapshot.pending_render_requests,
                    last_render_ms = presenter_snapshot.last_render_ms,
                    average_render_ms = presenter_snapshot.average_render_ms,
                    dropped_frames = presenter_snapshot.dropped_frames,
                    "video presenter output snapshot"
                );
            }

            match render_result {
                Ok(Some(frame)) => {
                    self.replace_visible_frame(render_image(frame), window, cx);
                }
                Ok(None) => {}
                Err(error) => {
                    let transition = self.session.render_failed(
                        error.to_string(),
                        BackendContext {
                            item_id: &self.emby.item_id,
                            media_source_id: &self.emby.media_source_id,
                            play_session_id: self.emby.play_session_id.as_deref(),
                            run_time_ticks: self.emby.run_time_ticks,
                            has_frame: self.presentation.frame.current.is_some(),
                        },
                    );
                    self.cancel_queue_switch();
                    self.execute_reporting_transition(transition, cx);
                    self.clear_visible_frame(window, cx);
                }
            }
        } else {
            self.clear_visible_frame(window, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::player::{PlaybackTrack, PlaybackTrackKind, SavedTrackChoice};
    use crate::settings::binding::PlaybackTrackPreferences;
    use tiny_playback::{
        ByteCacheState, DemuxCacheState, PlaybackCacheState, PlaybackCacheTimeRange,
    };

    use super::{
        BackendEvent, BackendEventKind, apply_cache_buffering_to_timeline,
        apply_paused_for_cache_to_timeline, apply_playback_restart_to_timeline,
        cache_state_needs_poll,
    };
    use crate::player::model::timeline::PlaybackTimelineState;

    #[gpui::test]
    fn playback_restart_closes_cache_popover_after_reducing_timeline(
        cx: &mut gpui::TestAppContext,
    ) {
        let (page, cx) = crate::player::page::test_support::playback_window(cx);
        cx.update(|window, cx| {
            page.update(cx, |page, cx| {
                page.presentation.timeline_presentation.cache_status_open = true;
                page.session.timeline_mut().pending_seek_position = Some(30.0);
                page.apply_backend_event(
                    BackendEvent::new(Default::default(), BackendEventKind::PlaybackRestart),
                    window,
                    cx,
                );
                assert!(!page.presentation.timeline_presentation.cache_status_open);
                assert!(page.session.timeline().loaded);
                assert_eq!(page.session.timeline().pending_seek_position, None);
            });
        });
    }

    #[gpui::test]
    fn subtitle_events_reuse_images_and_clear_them_after_failure(cx: &mut gpui::TestAppContext) {
        use std::sync::Arc;
        use tiny_playback::{
            BackendSubtitleBitmap, BackendSubtitleCue, BgraImage, SharedBgraImage,
        };

        let (page, cx) = crate::player::page::test_support::playback_window(cx);
        let image = SharedBgraImage::new(BgraImage::new(vec![1, 2, 3, 128], 1, 1).unwrap());
        let cue = BackendSubtitleCue {
            text: String::new(),
            bitmaps: vec![BackendSubtitleBitmap {
                image: image.clone(),
                x: 0,
                y: 0,
                width: 1,
                height: 1,
                canvas_width: 1920,
                canvas_height: 1080,
            }],
            start_nsecs: 0,
            end_nsecs: 1_000_000_000,
        };
        cx.update(|window, cx| {
            page.update(cx, |page, cx| {
                for _ in 0..2 {
                    page.apply_backend_event(
                        BackendEvent::new(
                            Default::default(),
                            BackendEventKind::SubtitleChanged(Some(cue.clone())),
                        ),
                        window,
                        cx,
                    );
                }
                let rendered = page
                    .presentation
                    .subtitle
                    .images
                    .get(&image)
                    .unwrap()
                    .clone();
                page.apply_backend_event(
                    BackendEvent::new(
                        Default::default(),
                        BackendEventKind::SubtitleChanged(Some(cue.clone())),
                    ),
                    window,
                    cx,
                );
                assert!(Arc::ptr_eq(
                    &rendered,
                    page.presentation.subtitle.images.get(&image).unwrap()
                ));
                assert_eq!(rendered.as_bytes(0).unwrap(), image.image().bytes());
                page.apply_backend_event(
                    BackendEvent::new(
                        Default::default(),
                        BackendEventKind::LoadFailed("test failure".into()),
                    ),
                    window,
                    cx,
                );
                assert!(page.presentation.subtitle.active.is_none());
                assert!(page.presentation.subtitle.images.get(&image).is_none());
            });
        });
    }

    #[gpui::test]
    fn resolved_stream_tracks_update_menus_without_saving_automatic_subtitle_off(
        cx: &mut gpui::TestAppContext,
    ) {
        let (page, cx) = crate::player::page::test_support::playback_window(cx);
        cx.update(|window, cx| {
            page.update(cx, |page, cx| {
                let original_subtitle = PlaybackTrack::new(7, "Chinese Simplified (ASS)", false);
                let external = PlaybackTrack::new(8, "External subtitle", true)
                    .with_external_url(Some("https://example.invalid/sub.srt".into()));
                page.session.source_mut().tracks.audio =
                    vec![PlaybackTrack::new(5, "Original audio", false)];
                page.session.source_mut().tracks.subtitles =
                    vec![original_subtitle.clone(), external.clone()];
                page.session.source_mut().tracks.selected_audio_stream_index = Some(5);
                page.session
                    .source_mut()
                    .tracks
                    .selected_subtitle_stream_index = Some(7);
                page.session.source_mut().remember_subtitle_on_start = true;
                PlaybackTrackPreferences::remember(
                    &page.emby.server,
                    std::slice::from_ref(&page.session.source_view().track_preference_key),
                    PlaybackTrackKind::Subtitle,
                    Some(&original_subtitle),
                    cx,
                );
                let saved = PlaybackTrackPreferences::get(
                    &page.emby.server,
                    &page.session.source_view().track_preference_key,
                    cx,
                )
                .subtitle;
                let audio = vec![PlaybackTrack::new(1, "Warning audio", false)];
                page.apply_backend_event(
                    BackendEvent::new(
                        Default::default(),
                        BackendEventKind::PlaybackTracksChanged {
                            audio: audio.clone(),
                            subtitles: Vec::new(),
                            selected: crate::player::PlaybackTrackSelection {
                                audio_stream_index: Some(1),
                                ..Default::default()
                            },
                        },
                    ),
                    window,
                    cx,
                );
                assert_eq!(page.session.source_view().tracks.audio, audio);
                assert_eq!(page.session.source_view().tracks.subtitles, vec![external]);
                assert_eq!(
                    page.session
                        .source_view()
                        .tracks
                        .selected_audio_stream_index,
                    Some(1)
                );
                assert!(
                    page.session
                        .source_view()
                        .tracks
                        .selected_subtitle_stream_index
                        .is_none()
                );
                assert!(!page.session.source_view().remember_subtitle_on_start);
                page.apply_backend_event(
                    BackendEvent::new(Default::default(), BackendEventKind::PlaybackRestart),
                    window,
                    cx,
                );
                assert!(page.session.timeline().loaded);
                assert!(page.session.controls_view().error.is_none());
                assert_eq!(
                    PlaybackTrackPreferences::get(
                        &page.emby.server,
                        &page.session.source_view().track_preference_key,
                        cx,
                    )
                    .subtitle,
                    saved
                );
            });
        });
    }

    #[gpui::test]
    fn audio_fallback_keeps_valid_detail_subtitle_pending_until_playback_starts(
        cx: &mut gpui::TestAppContext,
    ) {
        let (page, cx) = crate::player::page::test_support::playback_window(cx);
        cx.update(|window, cx| {
            page.update(cx, |page, cx| {
                let subtitle = PlaybackTrack::new(7, "Chinese Simplified (ASS)", false);
                page.session.source_mut().tracks.subtitles = vec![subtitle.clone()];
                page.session
                    .source_mut()
                    .tracks
                    .selected_subtitle_stream_index = Some(7);
                page.session.source_mut().remember_subtitle_on_start = true;
                page.apply_backend_event(
                    BackendEvent::new(
                        Default::default(),
                        BackendEventKind::PlaybackTracksChanged {
                            audio: vec![PlaybackTrack::new(1, "Audio", false)],
                            subtitles: vec![subtitle.clone()],
                            selected: crate::player::PlaybackTrackSelection {
                                audio_stream_index: Some(1),
                                subtitle_stream_index: Some(7),
                                ..Default::default()
                            },
                        },
                    ),
                    window,
                    cx,
                );
                assert!(page.session.source_view().remember_subtitle_on_start);
                page.apply_backend_event(
                    BackendEvent::new(Default::default(), BackendEventKind::PlaybackRestart),
                    window,
                    cx,
                );
                assert_eq!(
                    PlaybackTrackPreferences::get(
                        &page.emby.server,
                        &page.session.source_view().track_preference_key,
                        cx,
                    )
                    .subtitle,
                    Some(SavedTrackChoice::from_track(Some(&subtitle)))
                );
            });
        });
    }

    #[gpui::test]
    fn detail_subtitle_is_saved_only_after_backend_start(cx: &mut gpui::TestAppContext) {
        let (page, cx) = crate::player::page::test_support::playback_window(cx);
        cx.update(|window, cx| {
            page.update(cx, |page, cx| {
                page.session.timeline_mut().loaded = false;
                page.session.source_mut().tracks.subtitles =
                    vec![PlaybackTrack::new(9, "Chinese Simplified (ASS)", false)];
                page.session
                    .source_mut()
                    .tracks
                    .selected_subtitle_stream_index = Some(9);
                page.session.source_mut().remember_subtitle_on_start = true;
                PlaybackTrackPreferences::remember(
                    &page.emby.server,
                    std::slice::from_ref(&page.session.source_view().track_preference_key),
                    PlaybackTrackKind::Subtitle,
                    None,
                    cx,
                );
                page.apply_backend_event(
                    BackendEvent::new(Default::default(), BackendEventKind::PositionChanged(1.0)),
                    window,
                    cx,
                );
                assert_eq!(
                    PlaybackTrackPreferences::get(
                        &page.emby.server,
                        &page.session.source_view().track_preference_key,
                        cx
                    )
                    .subtitle,
                    Some(SavedTrackChoice::Off)
                );

                page.apply_backend_event(
                    BackendEvent::new(Default::default(), BackendEventKind::PlaybackRestart),
                    window,
                    cx,
                );
                assert_eq!(
                    PlaybackTrackPreferences::get(
                        &page.emby.server,
                        &page.session.source_view().track_preference_key,
                        cx
                    )
                    .subtitle,
                    Some(SavedTrackChoice::from_track(
                        page.session.source_view().tracks.subtitles.first()
                    ))
                );
                assert!(!page.session.source_view().remember_subtitle_on_start);

                // A later player change must not be replaced by the consumed detail draft.
                PlaybackTrackPreferences::remember(
                    &page.emby.server,
                    std::slice::from_ref(&page.session.source_view().track_preference_key),
                    PlaybackTrackKind::Subtitle,
                    None,
                    cx,
                );
                page.apply_backend_event(
                    BackendEvent::new(Default::default(), BackendEventKind::PlaybackRestart),
                    window,
                    cx,
                );
                assert_eq!(
                    PlaybackTrackPreferences::get(
                        &page.emby.server,
                        &page.session.source_view().track_preference_key,
                        cx
                    )
                    .subtitle,
                    Some(SavedTrackChoice::Off)
                );
            });
        });
    }

    #[gpui::test]
    fn failed_or_aborted_playback_does_not_save_detail_subtitle(cx: &mut gpui::TestAppContext) {
        for failure in [
            Some(BackendEventKind::LoadFailed("unavailable".into())),
            Some(BackendEventKind::Fatal("decoder failed".into())),
            None,
        ] {
            let (page, cx) = crate::player::page::test_support::playback_window(cx);
            cx.update(|window, cx| {
                page.update(cx, |page, cx| {
                    page.session.timeline_mut().loaded = false;
                    page.session.source_mut().tracks.subtitles =
                        vec![PlaybackTrack::new(9, "Chinese Simplified (ASS)", false)];
                    page.session
                        .source_mut()
                        .tracks
                        .selected_subtitle_stream_index = Some(9);
                    page.session.source_mut().remember_subtitle_on_start = true;
                    PlaybackTrackPreferences::remember(
                        &page.emby.server,
                        std::slice::from_ref(&page.session.source_view().track_preference_key),
                        PlaybackTrackKind::Subtitle,
                        None,
                        cx,
                    );
                    if let Some(failure) = failure {
                        page.apply_backend_event(
                            BackendEvent::new(Default::default(), failure),
                            window,
                            cx,
                        );
                    } else {
                        page.close_playback_reporting(false, false);
                    }
                    page.apply_backend_event(
                        BackendEvent::new(Default::default(), BackendEventKind::PlaybackRestart),
                        window,
                        cx,
                    );
                    assert_eq!(
                        PlaybackTrackPreferences::get(
                            &page.emby.server,
                            &page.session.source_view().track_preference_key,
                            cx
                        )
                        .subtitle,
                        Some(SavedTrackChoice::Off)
                    );
                });
            });
        }
    }

    #[gpui::test]
    fn starting_automatic_subtitle_does_not_create_an_explicit_preference(
        cx: &mut gpui::TestAppContext,
    ) {
        let (page, cx) = crate::player::page::test_support::playback_window(cx);
        cx.update(|window, cx| {
            page.update(cx, |page, cx| {
                page.session.source_mut().tracks.subtitles =
                    vec![PlaybackTrack::new(10, "Chinese Simplified (ASS)", false)];
                page.session
                    .source_mut()
                    .tracks
                    .selected_subtitle_stream_index = Some(10);
                page.apply_backend_event(
                    BackendEvent::new(Default::default(), BackendEventKind::PlaybackRestart),
                    window,
                    cx,
                );
                assert!(
                    PlaybackTrackPreferences::get(
                        &page.emby.server,
                        &page.session.source_view().track_preference_key,
                        cx
                    )
                    .subtitle
                    .is_none()
                );
            });
        });
    }

    fn byte_cache_state(idle: bool, download_fraction: Option<f64>) -> ByteCacheState {
        ByteCacheState {
            ranges: Vec::new(),
            reader_fraction: None,
            download_fraction,
            cached_bytes: 0,
            content_length: None,
            disk_cache_enabled: false,
            idle,
            raw_input_rate: None,
            byte_level_seeks: 0,
            ..ByteCacheState::default()
        }
    }

    fn idle_demux_state() -> DemuxCacheState {
        DemuxCacheState {
            idle: true,
            ..DemuxCacheState::default()
        }
    }

    #[test]
    fn cache_state_poll_continues_while_cache_pause_is_active() {
        let state = PlaybackCacheState {
            demux: idle_demux_state(),
            byte: Some(byte_cache_state(true, None)),
            paused_for_cache: true,
            ..PlaybackCacheState::default()
        };

        assert!(cache_state_needs_poll(&state));
    }

    #[test]
    fn cache_state_poll_continues_while_byte_cache_is_active() {
        let state = PlaybackCacheState {
            demux: idle_demux_state(),
            byte: Some(byte_cache_state(false, None)),
            ..PlaybackCacheState::default()
        };

        assert!(cache_state_needs_poll(&state));
    }

    #[test]
    fn cache_state_poll_stops_when_byte_and_demux_cache_are_idle() {
        let state = PlaybackCacheState {
            demux: idle_demux_state(),
            byte: Some(byte_cache_state(true, None)),
            ..PlaybackCacheState::default()
        };

        assert!(!cache_state_needs_poll(&state));
    }

    #[test]
    fn cache_state_poll_continues_until_byte_cache_reports_idle_even_when_download_completed() {
        let state = PlaybackCacheState {
            demux: idle_demux_state(),
            byte: Some(byte_cache_state(false, Some(1.0))),
            ..PlaybackCacheState::default()
        };

        assert!(cache_state_needs_poll(&state));
    }

    #[test]
    fn cache_state_poll_continues_while_demux_cache_is_active() {
        let state = PlaybackCacheState {
            demux: DemuxCacheState::default(),
            byte: Some(byte_cache_state(true, None)),
            ..PlaybackCacheState::default()
        };

        assert!(cache_state_needs_poll(&state));
    }

    #[test]
    fn playback_restart_preserves_seekable_cache_ranges() {
        let cache_state = PlaybackCacheState {
            demux: DemuxCacheState {
                cache_end: Some(90.0),
                reader_pts: Some(60.0),
                seekable_ranges: vec![PlaybackCacheTimeRange {
                    start: 12.0,
                    end: 90.0,
                }],
                ..DemuxCacheState::default()
            },
            ..PlaybackCacheState::default()
        };
        let mut timeline = PlaybackTimelineState {
            loaded: true,
            buffering: true,
            cache_state: Some(cache_state.clone()),
            pending_seek_position: Some(60.0),
            pending_seek_keeps_frame: true,
            ..PlaybackTimelineState::default()
        };

        apply_playback_restart_to_timeline(&mut timeline);

        assert_eq!(timeline.cache_state.as_ref(), Some(&cache_state));
        assert!(timeline.loaded);
        assert!(!timeline.buffering);
        assert_eq!(timeline.pending_seek_position, None);
        assert!(!timeline.pending_seek_keeps_frame);
    }

    #[test]
    fn playback_restart_clears_uncached_seek_loader_after_backend_buffering_stops() {
        use crate::player::page::render::{PlaybackStatus, playback_status};

        let mut timeline = PlaybackTimelineState {
            loaded: true,
            user_paused: false,
            pending_seek_position: Some(120.0),
            buffering: false,
            ..PlaybackTimelineState::default()
        };
        assert_eq!(
            playback_status(&timeline, true, false, None),
            Some(PlaybackStatus::Loading)
        );

        apply_playback_restart_to_timeline(&mut timeline);

        assert_eq!(playback_status(&timeline, true, false, None), None);
    }

    #[test]
    fn playback_restart_preserves_user_pause_after_initial_load() {
        let mut timeline = PlaybackTimelineState {
            loaded: true,
            user_paused: true,
            paused: true,
            pending_seek_position: Some(60.0),
            ..PlaybackTimelineState::default()
        };

        apply_playback_restart_to_timeline(&mut timeline);

        assert!(timeline.user_paused);
        assert!(timeline.paused);
        assert_eq!(timeline.pending_seek_position, None);
    }

    #[test]
    fn cache_pause_event_updates_stored_cache_state_copy() {
        let mut timeline = PlaybackTimelineState {
            loaded: true,
            user_paused: false,
            paused: true,
            paused_for_cache: true,
            cache_buffering_percent: Some(42),
            cache_state: Some(PlaybackCacheState {
                demux: idle_demux_state(),
                paused_for_cache: true,
                buffering_percent: Some(42),
                ..PlaybackCacheState::default()
            }),
            ..PlaybackTimelineState::default()
        };

        apply_paused_for_cache_to_timeline(&mut timeline, false);

        assert!(!timeline.paused_for_cache);
        assert!(!timeline.paused);
        assert_eq!(timeline.cache_buffering_percent, None);
        let cache_state = timeline.cache_state.expect("cache state remains available");
        assert!(!cache_state.paused_for_cache);
        assert_eq!(cache_state.buffering_percent, None);
    }

    #[test]
    fn cache_buffering_event_updates_stored_cache_state_copy() {
        let mut timeline = PlaybackTimelineState {
            cache_state: Some(PlaybackCacheState {
                demux: idle_demux_state(),
                ..PlaybackCacheState::default()
            }),
            ..PlaybackTimelineState::default()
        };

        apply_cache_buffering_to_timeline(&mut timeline, Some(37));

        assert_eq!(timeline.cache_buffering_percent, Some(37));
        assert_eq!(
            timeline
                .cache_state
                .as_ref()
                .and_then(|state| state.buffering_percent),
            Some(37)
        );
    }
}
