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
