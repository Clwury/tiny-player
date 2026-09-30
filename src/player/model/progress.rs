use super::{
    time::{clamp_playback_position, valid_playback_duration, valid_playback_time},
    timeline::PlaybackTimelineState,
};
use tiny_playback::PlaybackCacheState;

/// Ephemeral read model derived from the authoritative timeline on render.
pub(in crate::player) struct ProgressTimelineViewModel {
    pub(in crate::player) current_time: String,
    pub(in crate::player) duration_time: String,
    pub(in crate::player) played_fraction: f32,
    pub(in crate::player) cached_seek_preview: Option<bool>,
    pub(in crate::player) forward_cache_fraction: Option<f32>,
    pub(in crate::player) cache_ranges: Vec<(f32, f32)>,
}

pub(in crate::player) fn view_model(
    timeline: &PlaybackTimelineState,
) -> Option<ProgressTimelineViewModel> {
    let duration = timeline.duration?;
    let position = timeline
        .progress_drag_position
        .or(timeline.position)
        .unwrap_or(0.0);
    Some(ProgressTimelineViewModel {
        current_time: format_playback_time(position),
        duration_time: format_playback_time(duration),
        played_fraction: progress_fraction(position, duration),
        cached_seek_preview: timeline.progress_drag_position.map(|target| {
            cached_seek_target(
                timeline.cache_state.as_ref(),
                timeline.buffered_until,
                timeline.position,
                target,
            )
        }),
        forward_cache_fraction: forward_cache_fraction(timeline),
        cache_ranges: cache_range_fractions(timeline.cache_state.as_ref(), duration),
    })
}

pub(in crate::player) fn progress_fraction(position: f64, duration: f64) -> f32 {
    let Some(duration) = valid_playback_duration(duration) else {
        return 0.0;
    };
    (clamp_playback_position(position, duration) / duration) as f32
}

pub(in crate::player) fn forward_cache_fraction(timeline: &PlaybackTimelineState) -> Option<f32> {
    let duration = timeline.duration.and_then(valid_playback_duration)?;
    let seek_position = timeline
        .progress_drag_position
        .or(timeline.pending_seek_position);
    let position = valid_playback_time(seek_position.or(timeline.position).unwrap_or(0.0))?;
    let buffered_until = if seek_position.is_some() {
        // A seek needs a retained recovery point. Preview only the range that
        // contains its target, without bridging gaps to the current reader.
        if let Some(cache_state) = &timeline.cache_state {
            cache_state
                .demux
                .seekable_ranges
                .iter()
                .filter(|range| {
                    range.start.is_finite()
                        && range.end.is_finite()
                        && range.start <= position
                        && range.end > position
                })
                .map(|range| range.end)
                .max_by(f64::total_cmp)?
        } else {
            if !cached_seek_target(None, timeline.buffered_until, timeline.position, position) {
                return None;
            }
            timeline.buffered_until?
        }
    } else {
        // BufferedChanged/CacheStateChanged report the active demux cache end.
        // During continuous playback, consumed packets are already in the
        // decoder/output queues: trimming their recovery points can move the
        // seekable start ahead of the playhead without losing forward data.
        timeline.buffered_until?
    };
    let end_fraction = progress_fraction(valid_playback_time(buffered_until)?, duration);
    (end_fraction > progress_fraction(position, duration)).then_some(end_fraction)
}

pub(in crate::player) fn cache_range_fractions(
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

pub(in crate::player) fn cached_seek_target(
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

pub(in crate::player) fn buffered_until_after_seek(
    previous: Option<f64>,
    position: f64,
) -> Option<f64> {
    let position = valid_playback_time(position)?;
    Some(
        previous
            .and_then(valid_playback_time)
            .unwrap_or(position)
            .max(position),
    )
}

pub(in crate::player) fn format_playback_time(seconds: f64) -> String {
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
