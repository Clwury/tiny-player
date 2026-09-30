use super::*;
use crate::player::session::tests::controller;
use tiny_playback::{BackendError, PlaybackCacheState, PlaybackCacheTimeRange};

fn ready() -> PlaybackSessionController {
    let mut session = controller();
    session.timeline.loaded = true;
    session.timeline.user_paused = false;
    session.timeline.paused = false;
    session
}

fn reject(_: ControlCommand) -> Option<tiny_playback::Result<()>> {
    Some(Err(BackendError::Ffmpeg("rejected".into())))
}

#[test]
fn pause_intents_preserve_cache_pause_and_missing_or_rejected_commands() {
    for cache_paused in [false, true] {
        let mut session = ready();
        session.timeline.paused_for_cache = cache_paused;
        session.timeline.paused = cache_paused;
        let missing = session.dispatch_control(PlaybackIntent::TogglePause, |_| None);
        assert!(!missing.notify && !missing.report_progress);
        assert!(!session.timeline.user_paused);
        let paused = session.dispatch_control(PlaybackIntent::TogglePause, |effect| {
            assert_eq!(effect.command, BackendCommand::Pause);
            assert!(!effect.discard_frames);
            Some(Ok(()))
        });
        assert!(paused.report_progress && session.timeline.user_paused && session.timeline.paused);
        let rejected = session.dispatch_control(PlaybackIntent::TogglePause, reject);
        assert!(!rejected.report_progress && rejected.notify);
        assert!(session.timeline.user_paused && session.timeline.paused);
        assert_eq!(
            session.controls_view().error,
            Some("控制播放失败：rejected")
        );
        session.controls.error = None;
        let resumed = session.dispatch_control(PlaybackIntent::TogglePause, |effect| {
            assert_eq!(effect.command, BackendCommand::Resume);
            Some(Ok(()))
        });
        assert!(resumed.report_progress && !session.timeline.user_paused);
        assert_eq!(session.timeline.paused, cache_paused);
    }
}

#[test]
fn disabled_pause_and_relative_seek_do_not_dispatch_native_commands() {
    for reason in 0..3 {
        let mut session = ready();
        match reason {
            0 => session.timeline.loaded = false,
            1 => session.timeline.ended = true,
            _ => session.controls.error = Some("failed".into()),
        }
        for intent in [
            PlaybackIntent::TogglePause,
            PlaybackIntent::SeekRelative(10.0),
        ] {
            let update = session.dispatch_control(intent, |_| panic!("disabled control executed"));
            assert!(!update.notify);
        }
        assert!(!session.controls_view().can_toggle && !session.controls_view().can_seek);
    }
    let mut session = ready();
    session.timeline.duration = None;
    assert!(session.controls_view().can_toggle && !session.controls_view().can_seek);
}

#[test]
fn volume_can_be_saved_without_backend_and_mute_restores_last_level() {
    let mut session = ready();
    let muted = session.dispatch_control(PlaybackIntent::ToggleMute, |_| None);
    let settings = muted.volume_changed.unwrap();
    assert_eq!(settings.level, 0.0);
    assert_eq!(settings.unmuted_level, 0.75);
    assert!(muted.show_volume && muted.report_progress);
    let restored = session.dispatch_control(PlaybackIntent::ToggleMute, |_| Some(Ok(())));
    assert_eq!(restored.volume_changed.unwrap().level, 0.75);
    assert!(restored.report_progress);
    let changed = session.dispatch_control(PlaybackIntent::AdjustVolume(-0.25), |_| None);
    assert_eq!(changed.volume_changed.unwrap().level, 0.5);
    assert!(!changed.report_progress);
    let rejected = session.dispatch_control(PlaybackIntent::AdjustVolume(0.2), reject);
    assert!(rejected.show_volume && rejected.volume_changed.is_none());
    assert_eq!(session.controls_view().volume.level, 0.5);
    let unchanged = session.dispatch_control(PlaybackIntent::AdjustVolume(0.0), |_| {
        panic!("unchanged volume executed")
    });
    assert!(unchanged.show_volume && unchanged.volume_changed.is_none());
}

#[test]
fn rate_is_committed_only_after_success_and_remains_within_engine_limits() {
    let mut session = ready();
    for missing in [true, false] {
        let update = session.dispatch_control(
            PlaybackIntent::ChangeRate(PlaybackRateChange::Double),
            |effect| {
                assert_eq!(
                    effect.command,
                    BackendCommand::SetPlaybackRate { rate: 2.0 }
                );
                if missing { None } else { reject(effect) }
            },
        );
        assert!(!update.show_rate);
        assert_eq!(session.controls_view().rate, 1.0);
    }
    for _ in 0..5 {
        let update = session.dispatch_control(
            PlaybackIntent::ChangeRate(PlaybackRateChange::Double),
            |_| Some(Ok(())),
        );
        assert!(update.show_rate && !update.report_progress);
    }
    assert_eq!(session.controls_view().rate, 4.0);
}

#[test]
fn seek_uses_authoritative_cache_ranges_and_preserves_position_precision() {
    for (ranges, cached) in [
        (vec![], false),
        (
            vec![PlaybackCacheTimeRange {
                start: 1.0,
                end: 80.0,
            }],
            true,
        ),
    ] {
        let mut session = ready();
        session.timeline.buffered_until = Some(80.0);
        let mut cache = PlaybackCacheState::default();
        cache.demux.seekable_ranges = ranges;
        session.timeline.cache_state = Some(cache);
        let target = 30.1234567;
        let update = session.dispatch_control(
            PlaybackIntent::Seek {
                position: target,
                mode: PlaybackSeekMode::Precise,
            },
            |effect| {
                assert!(effect.discard_frames);
                assert_eq!(
                    effect.command,
                    BackendCommand::Seek {
                        position_seconds: target,
                        mode: PlaybackSeekMode::Precise
                    }
                );
                Some(Ok(()))
            },
        );
        assert!(update.report_progress);
        assert_eq!(session.timeline.pending_seek_position, Some(target));
        assert_eq!(session.timeline.position, Some(target));
        assert_eq!(session.timeline.pending_seek_keeps_frame, cached);
        assert_eq!(session.timeline.buffering, !cached);
        assert_eq!(
            session.timeline.buffered_until,
            Some(if cached { 80.0 } else { target })
        );
    }
}

#[test]
fn rejected_seek_restores_position_buffer_and_end_but_clears_drag_and_pending_seek() {
    for missing in [true, false] {
        let mut session = ready();
        session.timeline.buffered_until = Some(70.0);
        session.timeline.progress_drag_position = Some(12.0);
        session.timeline.pending_seek_position = Some(15.0);
        session.timeline.pending_seek_keeps_frame = true;
        session.timeline.ended = true;
        let update = session.dispatch_control(
            PlaybackIntent::Seek {
                position: 40.0,
                mode: PlaybackSeekMode::Precise,
            },
            |effect| if missing { None } else { reject(effect) },
        );
        assert!(!update.report_progress);
        assert_eq!(session.timeline.position, Some(2.5));
        assert_eq!(session.timeline.buffered_until, Some(70.0));
        assert!(session.timeline.ended);
        assert!(
            session.timeline.progress_drag_position.is_none()
                && session.timeline.pending_seek_position.is_none()
        );
        assert!(!session.timeline.pending_seek_keeps_frame && !session.timeline.buffering);
        assert_eq!(session.controls_view().error.is_some(), !missing);
    }
}

#[test]
fn relative_seek_prefers_preview_then_pending_then_position_and_clamps_to_duration() {
    let mut session = ready();
    session.timeline.progress_drag_position = Some(80.0);
    session.timeline.pending_seek_position = Some(40.0);
    for (delta, expected) in [(20.0, 90.0), (-10.0, 80.0)] {
        session.dispatch_control(PlaybackIntent::SeekRelative(delta), |effect| {
            assert_eq!(
                effect.command,
                BackendCommand::Seek {
                    position_seconds: expected,
                    mode: PlaybackSeekMode::Fast
                }
            );
            Some(Ok(()))
        });
    }
    session.timeline.pending_seek_position = None;
    session.dispatch_control(PlaybackIntent::SeekRelative(-200.0), |effect| {
        assert_eq!(
            effect.command,
            BackendCommand::Seek {
                position_seconds: 0.0,
                mode: PlaybackSeekMode::Fast
            }
        );
        Some(Ok(()))
    });
}

#[test]
fn track_failures_keep_choices_and_subtitle_resources_and_success_returns_effects() {
    let mut session = ready();
    session.timeline.progress_drag_position = Some(12.3456789);
    for intent in [
        PlaybackIntent::SelectAudio(Some(7)),
        PlaybackIntent::SelectSubtitle(None),
    ] {
        let update = session.dispatch_control(intent, reject);
        assert!(update.close_track_menu && update.notify);
        assert!(
            !update.clear_subtitle && !update.report_progress && update.remember_track.is_none()
        );
        assert_eq!(session.controls_view().audio.selected, Some(2));
        assert_eq!(session.controls_view().subtitles.selected, Some(4));
    }
    session.timeline.buffering = true;
    let update = session.dispatch_control(PlaybackIntent::SelectSubtitle(None), |effect| {
        assert_eq!(
            effect.command,
            BackendCommand::SetSubtitleTrack {
                track: None,
                position_seconds: 12.3456789
            }
        );
        Some(Ok(()))
    });
    assert!(update.clear_subtitle && update.report_progress);
    assert_eq!(update.remember_track, Some(PlaybackTrackKind::Subtitle));
    assert!(session.timeline.buffering); // Turning a track off retains existing buffering.
    assert_eq!(session.controls_view().subtitles.selected, None);
    let update = session.dispatch_control(PlaybackIntent::SelectAudio(Some(7)), |_| Some(Ok(())));
    assert_eq!(session.controls_view().audio.selected, Some(7));
    assert!(update.report_progress && !update.clear_subtitle);
}

#[test]
fn preview_threshold_and_borrowed_view_preserve_controls_and_progress_rules() {
    let mut session = ready();
    session.source.tracks.audio = vec![PlaybackTrack::new(2, "track", false)];
    for (position, changed) in [(30.0, true), (30.01, false), (30.03, true)] {
        let update = session.dispatch_control(PlaybackIntent::PreviewSeek(position), |_| {
            panic!("preview executed backend")
        });
        assert_eq!(update.notify, changed);
    }
    let view = session.controls_view();
    assert!(view.audio.enabled() && view.subtitles.enabled());
    assert_eq!(
        view.audio.tracks.as_ptr(),
        session.source.tracks.audio.as_ptr()
    );
    assert!(!view.can_previous && view.can_next);
    let progress = view.progress().unwrap();
    assert_eq!(progress.current_time, "0:30");
    assert_eq!(progress.duration_time, "1:30");
    assert_eq!(progress.played_fraction, (30.03 / 90.0) as f32);
    assert_eq!(progress.cached_seek_preview, Some(false));
}

#[test]
fn track_preferences_cover_requested_grouped_and_resolved_keys_without_duplicates() {
    let mut session = ready();
    assert!(
        session
            .track_preference_update(PlaybackTrackKind::Audio, "physical", "source")
            .is_none()
    );
    session.source.tracks.audio = vec![PlaybackTrack::new(2, "track", false)];
    let update = session
        .track_preference_update(PlaybackTrackKind::Audio, "resolved", "resolved-source")
        .unwrap();
    assert_eq!(update.track.unwrap().stream_index, 2);
    let keys: Vec<_> = update
        .keys
        .iter()
        .map(|key| (key.item_id.as_str(), key.media_source_id.as_str()))
        .collect();
    assert_eq!(
        keys,
        [
            ("physical", "source"),
            ("resolved", "source"),
            ("resolved", "resolved-source"),
            ("grouped-0", "source"),
            ("grouped-0", "resolved-source")
        ]
    );
    session.source.tracks.selected_audio_stream_index = None;
    assert!(
        session
            .track_preference_update(PlaybackTrackKind::Audio, "physical", "source")
            .unwrap()
            .track
            .is_none()
    );
}
