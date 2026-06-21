use super::super::nsecs_to_seconds;

pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn ordered_duration_seconds(
    reader_nsecs: Option<u64>,
    cache_end_nsecs: Option<u64>,
) -> Option<f64> {
    let (reader_nsecs, cache_end_nsecs) = reader_nsecs.zip(cache_end_nsecs)?;
    (cache_end_nsecs >= reader_nsecs)
        .then(|| nsecs_to_seconds(cache_end_nsecs.saturating_sub(reader_nsecs)))
}
