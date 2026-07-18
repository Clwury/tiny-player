use super::*;

pub(super) struct ProgressBarDrag;

impl Render for ProgressBarDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().hidden()
    }
}

pub(super) fn playback_status_message(buffering: bool, has_visible_frame: bool) -> SharedString {
    if buffering {
        "正在缓冲视频…".into()
    } else if has_visible_frame {
        "".into()
    } else {
        "正在加载视频…".into()
    }
}

pub(super) fn valid_playback_time(time: f64) -> Option<f64> {
    (time.is_finite() && time >= 0.0).then_some(time)
}

pub(super) fn valid_playback_duration(duration: f64) -> Option<f64> {
    (duration.is_finite() && duration > 0.0).then_some(duration)
}

pub(super) fn clamp_playback_position(position: f64, duration: f64) -> f64 {
    if !position.is_finite() {
        return 0.0;
    }
    position.clamp(0.0, duration.max(0.0))
}

pub(super) fn progress_fraction(position: f64, duration: f64) -> f32 {
    let Some(duration) = valid_playback_duration(duration) else {
        return 0.0;
    };
    (clamp_playback_position(position, duration) / duration) as f32
}

pub(super) fn cache_range_fractions(
    cache_state: Option<&PlaybackCacheState>,
    duration: f64,
) -> Vec<(f32, f32)> {
    let Some(duration) = valid_playback_duration(duration) else {
        return Vec::new();
    };
    let Some(cache_state) = cache_state else {
        return Vec::new();
    };
    // Mirror mpv OSC's seekRangesF: map each authoritative demux range directly
    // onto the duration. The FFmpeg demux report already coalesces positively
    // overlapping physical ranges like mpv's cache range joining. Clamping
    // happens only when the range is translated to track coordinates for drawing.
    cache_state
        .demux
        .seekable_ranges
        .iter()
        .filter_map(|range| normalized_cache_range(range.start, range.end, duration))
        .map(|(start, end)| ((start / duration) as f32, (end / duration) as f32))
        .collect()
}

fn normalized_cache_range(start: f64, end: f64, duration: f64) -> Option<(f64, f64)> {
    if !start.is_finite() || !end.is_finite() || !duration.is_finite() || duration <= 0.0 {
        return None;
    }
    (end > start).then_some((start, end))
}

pub(super) fn cached_seek_target(
    cache_state: Option<&PlaybackCacheState>,
    buffered_until: Option<f64>,
    reader_position: Option<f64>,
    target: f64,
) -> bool {
    let Some(target) = valid_playback_time(target) else {
        return false;
    };
    if let Some(cache_state) = cache_state {
        let ranges = &cache_state.demux.seekable_ranges;
        return ranges.iter().any(|range| {
            range.start.is_finite()
                && range.end.is_finite()
                && target >= range.start
                && target <= range.end
        });
    }

    let Some(reader_position) = reader_position.and_then(valid_playback_time) else {
        return false;
    };
    let Some(buffered_until) = buffered_until.and_then(valid_playback_time) else {
        return false;
    };
    target >= reader_position && target <= buffered_until
}

pub(super) fn buffered_until_after_seek(previous: Option<f64>, position: f64) -> Option<f64> {
    let position = valid_playback_time(position)?;
    Some(
        previous
            .and_then(valid_playback_time)
            .unwrap_or(position)
            .max(position),
    )
}

pub(super) fn should_apply_backend_position(
    progress_drag_position: Option<f64>,
    pending_seek_position: Option<f64>,
) -> bool {
    progress_drag_position.is_none() && pending_seek_position.is_none()
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

pub(super) fn format_playback_time(seconds: f64) -> String {
    let seconds = valid_playback_time(seconds).unwrap_or(0.0).round() as u64;
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

#[cfg(test)]
mod tests {
    use gpui::{Bounds, point, px, size};

    use crate::player::backend::{
        ByteCacheState, DemuxCacheState, PlaybackCacheByteRange, PlaybackCacheState,
        PlaybackCacheTimeRange,
    };

    use super::*;

    #[test]
    fn playback_status_stays_visible_until_first_frame() {
        assert_eq!(playback_status_message(true, false), "正在缓冲视频…");
        assert_eq!(playback_status_message(false, false), "正在加载视频…");
        assert_eq!(playback_status_message(false, true), "");
    }

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
