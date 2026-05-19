use super::*;

const HTTP_STREAM_BUFFER_POSITION_TOLERANCE: f64 = 0.02;
const PLAYBACK_BUFFER_POSITION_TOLERANCE_SECONDS: f64 = 0.05;

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

pub(super) fn valid_http_stream_buffer_progress(
    progress: HttpStreamBufferProgress,
) -> Option<HttpStreamBufferProgress> {
    if !progress.start_fraction.is_finite() || !progress.end_fraction.is_finite() {
        return None;
    }

    let start_fraction = progress.start_fraction.clamp(0.0, 1.0);
    let end_fraction = progress.end_fraction.clamp(0.0, 1.0);
    (end_fraction >= start_fraction).then_some(HttpStreamBufferProgress {
        start_fraction,
        end_fraction,
    })
}

pub(super) fn http_stream_buffered_range_fractions(
    progress: Option<HttpStreamBufferProgress>,
    continuous_until_fraction: f32,
) -> Option<(f32, f32)> {
    let progress = progress.and_then(valid_http_stream_buffer_progress)?;
    let continuous_until_fraction = continuous_until_fraction.clamp(0.0, 1.0);
    let start_fraction = (progress.start_fraction as f32).min(continuous_until_fraction);
    let end_fraction = progress.end_fraction as f32;
    (end_fraction > continuous_until_fraction).then_some((start_fraction, end_fraction))
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

pub(super) fn buffered_progress_fraction(
    buffered_until: Option<f64>,
    position: f64,
    duration: f64,
) -> f32 {
    let buffered_until = buffered_until.unwrap_or(position).max(position);
    progress_fraction(buffered_until, duration)
}

pub(super) fn should_apply_backend_position(
    progress_drag_position: Option<f64>,
    pending_seek_position: Option<f64>,
) -> bool {
    progress_drag_position.is_none() && pending_seek_position.is_none()
}

pub(super) fn is_seek_position_buffered(
    position: f64,
    playback_position: Option<f64>,
    playback_buffered_until: Option<f64>,
    http_stream_buffered_range: Option<HttpStreamBufferProgress>,
    duration: Option<f64>,
) -> bool {
    let Some(duration) = duration.and_then(valid_playback_duration) else {
        return false;
    };
    let position = clamp_playback_position(position, duration);
    let playback_position = playback_position
        .and_then(valid_playback_time)
        .map(|position| clamp_playback_position(position, duration))
        .unwrap_or(position);

    let playback_buffered = playback_buffered_until
        .and_then(valid_playback_time)
        .is_some_and(|buffered_until| {
            let buffered_until = clamp_playback_position(buffered_until, duration);
            position + PLAYBACK_BUFFER_POSITION_TOLERANCE_SECONDS >= playback_position
                && position <= buffered_until + PLAYBACK_BUFFER_POSITION_TOLERANCE_SECONDS
        });
    let http_stream_buffered =
        http_stream_buffered_until(http_stream_buffered_range, position, duration).is_some_and(
            |buffered_until| {
                position <= buffered_until + PLAYBACK_BUFFER_POSITION_TOLERANCE_SECONDS
            },
        );

    playback_buffered || http_stream_buffered
}

#[cfg(test)]
pub(super) fn combined_buffered_until(
    playback_buffered_until: Option<f64>,
    http_stream_buffered_range: Option<HttpStreamBufferProgress>,
    position: f64,
    duration: f64,
) -> Option<f64> {
    let http_stream_buffered_until =
        http_stream_buffered_until(http_stream_buffered_range, position, duration);
    match (
        playback_buffered_until.and_then(valid_playback_time),
        http_stream_buffered_until,
    ) {
        (Some(playback), Some(http_stream)) => Some(playback.max(http_stream)),
        (Some(playback), None) => Some(playback),
        (None, Some(http_stream)) => Some(http_stream),
        (None, None) => None,
    }
}

pub(super) fn http_stream_buffered_until(
    progress: Option<HttpStreamBufferProgress>,
    position: f64,
    duration: f64,
) -> Option<f64> {
    let duration = valid_playback_duration(duration)?;
    let progress = progress.and_then(valid_http_stream_buffer_progress)?;
    let position_fraction = progress_fraction(position, duration) as f64;
    if position_fraction + HTTP_STREAM_BUFFER_POSITION_TOLERANCE < progress.start_fraction
        || position_fraction > progress.end_fraction + HTTP_STREAM_BUFFER_POSITION_TOLERANCE
    {
        return None;
    }

    Some(duration * progress.end_fraction)
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

    use crate::player::backend::HttpStreamBufferProgress;

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
    fn buffered_progress_never_falls_behind_played_progress() {
        assert_eq!(buffered_progress_fraction(Some(20.0), 40.0, 100.0), 0.4);
        assert_eq!(buffered_progress_fraction(Some(80.0), 40.0, 100.0), 0.8);
        assert_eq!(buffered_progress_fraction(None, 40.0, 100.0), 0.4);
    }

    #[test]
    fn backend_position_updates_wait_until_seek_finishes() {
        assert!(should_apply_backend_position(None, None));
        assert!(!should_apply_backend_position(Some(40.0), None));
        assert!(!should_apply_backend_position(None, Some(80.0)));
    }

    #[test]
    fn cached_seek_detects_playback_and_http_buffered_ranges() {
        assert!(is_seek_position_buffered(
            60.0,
            Some(40.0),
            Some(80.0),
            None,
            Some(100.0),
        ));
        assert!(!is_seek_position_buffered(
            20.0,
            Some(40.0),
            Some(80.0),
            None,
            Some(100.0),
        ));

        let http_range = Some(HttpStreamBufferProgress {
            start_fraction: 0.2,
            end_fraction: 0.8,
        });
        assert!(is_seek_position_buffered(
            30.0,
            Some(90.0),
            None,
            http_range,
            Some(100.0),
        ));
        assert!(!is_seek_position_buffered(
            90.0,
            Some(10.0),
            None,
            http_range,
            Some(100.0),
        ));
    }

    #[test]
    fn http_stream_buffer_progress_validates_and_clamps_fraction_range() {
        assert_eq!(
            valid_http_stream_buffer_progress(HttpStreamBufferProgress {
                start_fraction: -0.5,
                end_fraction: 1.5,
            }),
            Some(HttpStreamBufferProgress {
                start_fraction: 0.0,
                end_fraction: 1.0,
            })
        );
        assert_eq!(
            valid_http_stream_buffer_progress(HttpStreamBufferProgress {
                start_fraction: 0.8,
                end_fraction: 0.2,
            }),
            None
        );
        assert_eq!(
            valid_http_stream_buffer_progress(HttpStreamBufferProgress {
                start_fraction: f64::NAN,
                end_fraction: 0.2,
            }),
            None
        );
    }

    #[test]
    fn http_stream_buffer_progress_only_applies_to_current_playback_range() {
        let range = Some(HttpStreamBufferProgress {
            start_fraction: 0.4,
            end_fraction: 0.7,
        });

        assert_eq!(http_stream_buffered_until(range, 50.0, 100.0), Some(70.0));
        assert_eq!(http_stream_buffered_until(range, 10.0, 100.0), None);
        assert_eq!(http_stream_buffered_until(range, 90.0, 100.0), None);
    }

    #[test]
    fn http_stream_buffer_progress_range_fills_gap_from_continuous_cache() {
        let range = Some(HttpStreamBufferProgress {
            start_fraction: 0.4,
            end_fraction: 0.7,
        });

        assert_eq!(
            http_stream_buffered_range_fractions(range, 0.1),
            Some((0.1, 0.7))
        );
        assert_eq!(
            http_stream_buffered_range_fractions(range, 0.5),
            Some((0.4, 0.7))
        );
        assert_eq!(http_stream_buffered_range_fractions(range, 0.8), None);
        assert_eq!(http_stream_buffered_until(range, 10.0, 100.0), None);
    }

    #[test]
    fn combined_buffered_progress_uses_furthest_playable_buffer() {
        let range = Some(HttpStreamBufferProgress {
            start_fraction: 0.2,
            end_fraction: 0.8,
        });

        assert_eq!(
            combined_buffered_until(Some(30.0), range, 40.0, 100.0),
            Some(80.0)
        );
        assert_eq!(
            combined_buffered_until(Some(90.0), range, 40.0, 100.0),
            Some(90.0)
        );
        assert_eq!(
            combined_buffered_until(None, range, 40.0, 100.0),
            Some(80.0)
        );
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
