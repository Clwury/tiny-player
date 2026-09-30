use super::*;
pub(super) use crate::player::model::progress::format_playback_time;
pub(super) use crate::player::model::time::{
    clamp_playback_position, valid_playback_duration, valid_playback_time,
};

pub(super) struct ProgressBarDrag;

impl Render for ProgressBarDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().hidden()
    }
}

pub(super) fn progress_fraction_for_cursor(
    cursor_x: Pixels,
    bounds: Bounds<Pixels>,
) -> Option<f32> {
    let width = f32::from(bounds.size.width);
    if width <= 0.0 {
        return None;
    }

    Some(((f32::from(cursor_x) - f32::from(bounds.origin.x)) / width).clamp(0.0, 1.0))
}

#[cfg(test)]
mod tests {
    use crate::player::model::time::should_apply_backend_position;
    use gpui::{Bounds, point, px, size};

    use tiny_playback::{
        ByteCacheState, DemuxCacheState, PlaybackCacheByteRange, PlaybackCacheState,
        PlaybackCacheTimeRange,
    };

    use super::*;
    use crate::player::model::progress::{
        buffered_until_after_seek, cache_range_fractions, cached_seek_target,
        forward_cache_fraction, progress_fraction,
    };

    #[test]
    fn playback_time_helpers_reject_invalid_values() {
        assert_eq!(valid_playback_time(12.0), Some(12.0));
        assert_eq!(valid_playback_time(-1.0), None);
        assert_eq!(valid_playback_time(f64::NAN), None);
        assert_eq!(valid_playback_duration(12.0), Some(12.0));
        assert_eq!(valid_playback_duration(0.0), None);
    }

    #[test]
    fn progress_fraction_clamps_position_to_duration() {
        assert_eq!(clamp_playback_position(-5.0, 100.0), 0.0);
        assert_eq!(clamp_playback_position(25.0, 100.0), 25.0);
        assert_eq!(clamp_playback_position(125.0, 100.0), 100.0);
        assert_eq!(progress_fraction(25.0, 100.0), 0.25);
        assert_eq!(progress_fraction(125.0, 100.0), 1.0);
        assert_eq!(progress_fraction(25.0, 0.0), 0.0);
    }

    #[test]
    fn forward_cache_fill_survives_logged_seekable_start_jumps() {
        // tiny-vk.log, 2026-09-13 07:22:48Z and 07:23:02Z: backbuffer
        // trimming advanced the seekable start while ~42s remained ahead.
        for (position, previous_start, trimmed_start, seekable_end, buffered_until) in [
            (82.8, 69.666666667, 84.3, 124.6, 127.616870748),
            (95.9, 84.3, 97.733333333, 140.2, 141.177324263),
        ] {
            let mut timeline = PlaybackTimelineState {
                position: Some(position),
                duration: Some(1200.0),
                buffered_until: Some(buffered_until),
                cache_state: Some(PlaybackCacheState {
                    demux: DemuxCacheState {
                        cache_end: Some(buffered_until),
                        seekable_ranges: vec![PlaybackCacheTimeRange {
                            start: previous_start,
                            end: seekable_end,
                        }],
                        ..DemuxCacheState::default()
                    },
                    ..PlaybackCacheState::default()
                }),
                ..PlaybackTimelineState::default()
            };
            let expected = Some((buffered_until / 1200.0) as f32);
            assert_eq!(forward_cache_fraction(&timeline), expected);

            timeline.cache_state.as_mut().unwrap().demux.seekable_ranges[0].start = trimmed_start;
            assert!(!cached_seek_target(
                timeline.cache_state.as_ref(),
                timeline.buffered_until,
                timeline.position,
                position,
            ));
            assert_eq!(forward_cache_fraction(&timeline), expected);

            // Crossing the new recovery point must not toggle the fill.
            timeline.position = Some(trimmed_start + 0.1);
            assert_eq!(forward_cache_fraction(&timeline), expected);
        }
    }

    #[test]
    fn forward_cache_fill_tracks_active_buffer_without_waiting_for_seekable_ranges() {
        let mut timeline = PlaybackTimelineState {
            position: Some(0.0),
            duration: Some(100.0),
            buffered_until: Some(4.0),
            cache_state: Some(PlaybackCacheState::default()),
            ..PlaybackTimelineState::default()
        };
        assert_eq!(forward_cache_fraction(&timeline), Some(0.04));

        timeline.cache_state.as_mut().unwrap().demux.seekable_ranges =
            vec![PlaybackCacheTimeRange {
                start: 0.5,
                end: 4.0,
            }];
        timeline.buffered_until = Some(10.0);
        assert_eq!(forward_cache_fraction(&timeline), Some(0.1));

        // Follow real cache loss too; never keep an obsolete high-water mark
        // or fall back to the end of an archived seekable range.
        timeline.buffered_until = Some(2.0);
        assert_eq!(forward_cache_fraction(&timeline), Some(0.02));
        timeline.buffered_until = None;
        assert_eq!(forward_cache_fraction(&timeline), None);
    }

    #[test]
    fn forward_cache_seek_preview_stays_within_the_target_range() {
        let mut timeline = PlaybackTimelineState {
            position: Some(70.0),
            duration: Some(100.0),
            buffered_until: Some(90.0),
            cache_state: Some(PlaybackCacheState {
                demux: DemuxCacheState {
                    seekable_ranges: vec![
                        PlaybackCacheTimeRange {
                            start: 10.0,
                            end: 30.0,
                        },
                        PlaybackCacheTimeRange {
                            start: 60.0,
                            end: 85.0,
                        },
                    ],
                    ..DemuxCacheState::default()
                },
                ..PlaybackCacheState::default()
            }),
            ..PlaybackTimelineState::default()
        };
        for (target, expected) in [
            (15.0, Some(0.3)),
            (45.0, None),
            (70.0, Some(0.85)),
            (85.0, None),
            (95.0, None),
        ] {
            timeline.progress_drag_position = Some(target);
            assert_eq!(forward_cache_fraction(&timeline), expected);

            timeline.progress_drag_position = None;
            timeline.pending_seek_position = Some(target);
            assert_eq!(forward_cache_fraction(&timeline), expected);
            timeline.pending_seek_position = None;
        }
        assert_eq!(forward_cache_fraction(&timeline), Some(0.9));
    }

    #[test]
    fn forward_cache_seek_preview_without_demux_state_requires_forward_coverage() {
        let mut timeline = PlaybackTimelineState {
            position: Some(50.0),
            duration: Some(100.0),
            buffered_until: Some(70.0),
            ..PlaybackTimelineState::default()
        };
        assert_eq!(forward_cache_fraction(&timeline), Some(0.7));
        for (target, expected) in [(30.0, None), (60.0, Some(0.7)), (80.0, None)] {
            timeline.progress_drag_position = Some(target);
            assert_eq!(forward_cache_fraction(&timeline), expected);
        }
    }

    #[test]
    fn forward_cache_fill_clamps_to_duration_and_rejects_invalid_or_consumed_buffer() {
        let mut timeline = PlaybackTimelineState {
            position: Some(25.0),
            duration: Some(100.0),
            buffered_until: Some(120.0),
            ..PlaybackTimelineState::default()
        };
        assert_eq!(forward_cache_fraction(&timeline), Some(1.0));
        for value in [None, Some(-1.0), Some(f64::NAN), Some(f64::INFINITY)] {
            timeline.buffered_until = value;
            assert_eq!(forward_cache_fraction(&timeline), None);
            timeline.buffered_until = Some(120.0);
            timeline.duration = value;
            assert_eq!(forward_cache_fraction(&timeline), None);
            timeline.duration = Some(100.0);
        }
        timeline.duration = Some(0.0);
        assert_eq!(forward_cache_fraction(&timeline), None);
        timeline.duration = Some(100.0);
        for position in [100.0, 120.0, -1.0, f64::NAN, f64::INFINITY] {
            timeline.position = Some(position);
            assert_eq!(forward_cache_fraction(&timeline), None);
        }
        timeline.position = Some(25.0);
        for buffered_until in [0.0, 20.0, 25.0] {
            timeline.buffered_until = Some(buffered_until);
            assert_eq!(forward_cache_fraction(&timeline), None);
        }
    }

    #[test]
    fn cache_range_fractions_map_seekable_ranges_like_mpv_osc() {
        let state = PlaybackCacheState {
            demux: DemuxCacheState {
                seekable_ranges: vec![
                    PlaybackCacheTimeRange {
                        start: -10.0,
                        end: 20.0,
                    },
                    PlaybackCacheTimeRange {
                        start: 15.0,
                        end: 40.0,
                    },
                    PlaybackCacheTimeRange {
                        start: 80.0,
                        end: 120.0,
                    },
                ],
                ..DemuxCacheState::default()
            },
            ..PlaybackCacheState::default()
        };

        assert_eq!(
            cache_range_fractions(Some(&state), 100.0),
            vec![(-0.1, 0.2), (0.15, 0.4), (0.8, 1.2)]
        );
    }

    #[test]
    fn cache_range_fractions_prefers_seekable_ranges() {
        let state = PlaybackCacheState {
            demux: DemuxCacheState {
                cache_end: Some(45.0),
                reader_pts: Some(30.0),
                seekable_ranges: vec![
                    PlaybackCacheTimeRange {
                        start: 10.0,
                        end: 20.0,
                    },
                    PlaybackCacheTimeRange {
                        start: 40.0,
                        end: 50.0,
                    },
                ],
                ..DemuxCacheState::default()
            },
            ..PlaybackCacheState::default()
        };

        assert_eq!(
            cache_range_fractions(Some(&state), 100.0),
            vec![(0.1, 0.2), (0.4, 0.5)]
        );
    }

    #[test]
    fn cache_range_fractions_ignores_forward_cache_without_seekable_ranges() {
        let state = PlaybackCacheState {
            demux: DemuxCacheState {
                cache_end: Some(45.0),
                reader_pts: Some(30.0),
                ..DemuxCacheState::default()
            },
            ..PlaybackCacheState::default()
        };

        assert_eq!(
            cache_range_fractions(Some(&state), 100.0),
            Vec::<(f32, f32)>::new()
        );
    }

    #[test]
    fn cached_seek_target_uses_seekable_ranges_before_buffered_fallback() {
        let state = PlaybackCacheState {
            demux: DemuxCacheState {
                seekable_ranges: vec![
                    PlaybackCacheTimeRange {
                        start: 10.0,
                        end: 20.0,
                    },
                    PlaybackCacheTimeRange {
                        start: 40.0,
                        end: 50.0,
                    },
                ],
                ..DemuxCacheState::default()
            },
            ..PlaybackCacheState::default()
        };

        assert!(cached_seek_target(
            Some(&state),
            Some(100.0),
            Some(0.0),
            15.0
        ));
        assert!(!cached_seek_target(
            Some(&state),
            Some(100.0),
            Some(0.0),
            30.0
        ));
    }

    #[test]
    fn cached_seek_target_falls_back_to_forward_buffer_when_ranges_are_missing() {
        assert!(cached_seek_target(None, Some(50.0), Some(10.0), 30.0));
        assert!(!cached_seek_target(None, Some(50.0), Some(10.0), 5.0));
        assert!(!cached_seek_target(None, Some(50.0), Some(10.0), 60.0));
        assert!(!cached_seek_target(None, Some(f64::NAN), Some(10.0), 30.0));
    }

    #[test]
    fn authoritative_empty_seekable_ranges_ignore_byte_and_forward_cache() {
        let state = PlaybackCacheState {
            demux: DemuxCacheState {
                cache_end: Some(50.0),
                reader_pts: Some(10.0),
                forward_bytes: 32 * 1024 * 1024,
                ..DemuxCacheState::default()
            },
            byte: Some(ByteCacheState {
                ranges: vec![PlaybackCacheByteRange {
                    start_fraction: 0.0,
                    end_fraction: 1.0,
                }],
                cached_bytes: 128 * 1024 * 1024,
                ..ByteCacheState::default()
            }),
            ..PlaybackCacheState::default()
        };

        assert!(cache_range_fractions(Some(&state), 100.0).is_empty());
        assert!(!cached_seek_target(
            Some(&state),
            Some(50.0),
            Some(10.0),
            30.0,
        ));
    }

    #[test]
    fn buffered_until_keeps_cached_end_when_seek_target_is_buffered() {
        assert_eq!(buffered_until_after_seek(Some(80.0), 20.0), Some(80.0));
        assert_eq!(buffered_until_after_seek(Some(10.0), 40.0), Some(40.0));
        assert_eq!(buffered_until_after_seek(None, 12.0), Some(12.0));
        assert_eq!(buffered_until_after_seek(Some(f64::NAN), 12.0), Some(12.0));
    }

    #[test]
    fn backend_position_updates_wait_until_seek_finishes() {
        assert!(should_apply_backend_position(None, None));
        assert!(!should_apply_backend_position(Some(40.0), None));
        assert!(!should_apply_backend_position(None, Some(80.0)));
    }

    #[test]
    fn progress_cursor_fraction_uses_track_bounds_and_clamps_edges() {
        let bounds = Bounds::new(point(px(100.0), px(0.0)), size(px(400.0), px(28.0)));

        assert_eq!(progress_fraction_for_cursor(px(100.0), bounds), Some(0.0));
        assert_eq!(progress_fraction_for_cursor(px(300.0), bounds), Some(0.5));
        assert_eq!(progress_fraction_for_cursor(px(700.0), bounds), Some(1.0));
        assert_eq!(
            progress_fraction_for_cursor(
                px(100.0),
                Bounds::new(point(px(0.0), px(0.0)), size(px(0.0), px(28.0))),
            ),
            None
        );
    }

    #[test]
    fn format_playback_time_switches_to_hours_when_needed() {
        assert_eq!(format_playback_time(65.0), "1:05");
        assert_eq!(format_playback_time(3661.0), "1:01:01");
        assert_eq!(format_playback_time(f64::NAN), "0:00");
    }
}
