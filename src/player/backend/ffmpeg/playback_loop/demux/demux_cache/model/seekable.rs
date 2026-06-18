use super::super::nsecs_to_seconds;

pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn ordered_duration_seconds(
    reader_nsecs: Option<u64>,
    cache_end_nsecs: Option<u64>,
) -> Option<f64> {
    let (reader_nsecs, cache_end_nsecs) = reader_nsecs.zip(cache_end_nsecs)?;
    (cache_end_nsecs >= reader_nsecs)
        .then(|| nsecs_to_seconds(cache_end_nsecs.saturating_sub(reader_nsecs)))
}

#[derive(Default)]
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) struct SeekableTimelineSegment {
    seek_start_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) end_nsecs: Option<u64>,
    previous_recovery_start_nsecs: Option<u64>,
}

impl SeekableTimelineSegment {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn push_packet(
        &mut self,
        start_nsecs: u64,
        end_nsecs: u64,
        recovery_point: bool,
        cached_seek_preroll_nsecs: u64,
    ) {
        self.end_nsecs = Some(self.end_nsecs.unwrap_or(end_nsecs).max(end_nsecs));
        if !recovery_point {
            return;
        }

        if self.seek_start_nsecs.is_none() {
            self.seek_start_nsecs = if cached_seek_preroll_nsecs == 0 {
                Some(start_nsecs)
            } else {
                self.previous_recovery_start_nsecs.map(|previous_start| {
                    if previous_start == 0 {
                        start_nsecs
                    } else {
                        start_nsecs.max(previous_start.saturating_add(cached_seek_preroll_nsecs))
                    }
                })
            };
        }

        self.previous_recovery_start_nsecs = Some(start_nsecs);
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn finish_into(
        self,
        ranges: &mut Vec<(u64, u64)>,
    ) {
        let Some(start_nsecs) = self.seek_start_nsecs else {
            return;
        };
        let Some(end_nsecs) = self.end_nsecs else {
            return;
        };
        if end_nsecs > start_nsecs {
            ranges.push((start_nsecs, end_nsecs));
        }
    }
}
