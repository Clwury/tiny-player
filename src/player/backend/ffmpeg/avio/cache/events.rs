use crate::player::backend::{
    ByteCacheState, DemuxCacheState, PlaybackCacheByteRange, PlaybackCacheState,
};

use super::HTTP_CACHE_PROGRESS_REPORT_THRESHOLD;

fn http_stream_buffer_progress_changed(
    previous: Option<PlaybackCacheByteRange>,
    next: PlaybackCacheByteRange,
) -> bool {
    let Some(previous) = previous else {
        return true;
    };
    (previous.start_fraction - next.start_fraction).abs() >= HTTP_CACHE_PROGRESS_REPORT_THRESHOLD
        || (previous.end_fraction - next.end_fraction).abs() >= HTTP_CACHE_PROGRESS_REPORT_THRESHOLD
        || (next.end_fraction >= 1.0 && previous.end_fraction < 1.0)
}

pub(in crate::player::backend::ffmpeg::avio::cache) fn playback_cache_state_from_http_status(
    status: ByteCacheState,
) -> PlaybackCacheState {
    let raw_input_rate = status.raw_input_rate;
    let byte_level_seeks = status.byte_level_seeks;
    PlaybackCacheState {
        demux: DemuxCacheState {
            raw_input_rate,
            byte_level_seeks,
            ..DemuxCacheState::default()
        },
        byte: Some(status),
        ..PlaybackCacheState::default()
    }
}

pub(in crate::player::backend::ffmpeg::avio::cache) fn http_stream_cache_status_changed(
    previous: Option<&ByteCacheState>,
    next: &ByteCacheState,
    cached_bytes_threshold: u64,
) -> bool {
    let Some(previous) = previous else {
        return true;
    };
    if previous.disk_cache_enabled != next.disk_cache_enabled
        || previous.idle != next.idle
        || previous.content_length != next.content_length
        || previous.ranges.len() != next.ranges.len()
        || previous.raw_input_rate.is_some() != next.raw_input_rate.is_some()
        || previous.byte_level_seeks != next.byte_level_seeks
    {
        return true;
    }
    if previous
        .reader_fraction
        .zip(next.reader_fraction)
        .is_some_and(|(previous, next)| {
            (previous - next).abs() >= HTTP_CACHE_PROGRESS_REPORT_THRESHOLD
        })
    {
        return true;
    }
    if previous
        .download_fraction
        .zip(next.download_fraction)
        .is_some_and(|(previous, next)| {
            (previous - next).abs() >= HTTP_CACHE_PROGRESS_REPORT_THRESHOLD
        })
    {
        return true;
    }
    if previous
        .raw_input_rate
        .zip(next.raw_input_rate)
        .is_some_and(|(previous, next)| previous.abs_diff(next) >= 64 * 1024)
    {
        return true;
    }
    previous.cached_bytes.abs_diff(next.cached_bytes) >= cached_bytes_threshold
        || previous
            .ranges
            .iter()
            .zip(next.ranges.iter())
            .any(|(previous, next)| http_stream_buffer_progress_changed(Some(*previous), *next))
}
