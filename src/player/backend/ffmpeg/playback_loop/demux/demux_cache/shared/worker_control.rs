use std::time::Instant;

use super::{
    DEMUX_PACKET_CACHE_PREFETCH_PAUSE_LOG_AFTER, DEMUX_PACKET_CACHE_PREFETCH_PAUSE_LOG_INTERVAL,
    DEMUX_PACKET_CACHE_WAIT_INTERVAL, DemuxPacketCacheShared, DemuxSeekRequest,
    DemuxSelectedStreams, PlaybackSessionId, nsecs_to_seconds,
};

impl DemuxPacketCacheShared {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn should_skip_seek_request(
        &self,
        request: &DemuxSeekRequest,
    ) -> bool {
        request.seek_generation < self.control.seek_generation()
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn should_discard_demux_result(
        &self,
        generation: u64,
        seek_generation: u64,
    ) -> bool {
        self.generation() != generation || self.control.seek_generation() != seek_generation
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn should_stop(
        &self,
    ) -> bool {
        self.control.should_stop()
            || self
                .state
                .lock()
                .expect("FFmpeg demux packet cache poisoned")
                .shutdown
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn wait_for_demux_permit(
        &self,
    ) -> Option<DemuxSeekRequest> {
        let mut guard = self
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        let mut logged_prefetch_pause = false;
        let mut prefetch_pause_started_at = None;
        let mut next_prefetch_pause_log_at = None;
        loop {
            if guard.shutdown || self.control.should_stop() {
                return None;
            }
            if let Some(request) = guard.take_seek_request() {
                return Some(request);
            }
            if self.control.has_pending_seek() {
                return None;
            }
            if guard.read_range_eof() || guard.error.is_some() {
                let (next_guard, _) = self
                    .ready
                    .wait_timeout(guard, DEMUX_PACKET_CACHE_WAIT_INTERVAL)
                    .expect("FFmpeg demux packet cache poisoned");
                guard = next_guard;
                continue;
            }
            let should_pause_demux = guard.should_pause_demux();
            if !should_pause_demux {
                return None;
            }
            let now = Instant::now();
            let pause_started = *prefetch_pause_started_at.get_or_insert(now);
            if !logged_prefetch_pause {
                tracing::debug!(
                    session_id = ?guard.session_id,
                    read_index = guard.read_index,
                    packet_count = guard.read_range().global_order.len(),
                    cached_bytes = guard.cached_bytes,
                    forward_bytes = guard.forward_bytes(),
                    forward_duration_ms = guard.forward_duration_nsecs() as f64 / 1_000_000.0,
                    reader_nsecs = guard.reader_nsecs,
                    reader_pts_seconds = nsecs_to_seconds(guard.reader_nsecs),
                    cached_until_nsecs = ?guard.cached_until_nsecs(),
                    cache_end_seconds = ?guard.cached_until_nsecs().map(nsecs_to_seconds),
                    readahead_nsecs = ?guard
                        .cached_until_nsecs()
                        .map(|cached_until| cached_until.saturating_sub(guard.reader_nsecs)),
                    raw_input_rate_bytes_per_sec = ?guard.raw_input_rate(),
                    cache_pause_percent = ?guard.cache_pause_percent(),
                    cache_paused = self.control.is_cache_paused(),
                    should_pause_demux,
                    readahead_ms = guard.readahead_nsecs as f64 / 1_000_000.0,
                    generation = guard.generation,
                    seek_generation = self.control.seek_generation(),
                    "FFmpeg demux packet cache prefetch paused"
                );
                logged_prefetch_pause = true;
                next_prefetch_pause_log_at =
                    now.checked_add(DEMUX_PACKET_CACHE_PREFETCH_PAUSE_LOG_AFTER);
            } else if next_prefetch_pause_log_at.is_some_and(|deadline| now >= deadline) {
                tracing::debug!(
                    session_id = ?guard.session_id,
                    paused_ms = now.saturating_duration_since(pause_started).as_millis(),
                    read_index = guard.read_index,
                    packet_count = guard.read_range().global_order.len(),
                    cached_bytes = guard.cached_bytes,
                    forward_bytes = guard.forward_bytes(),
                    forward_duration_ms = guard.forward_duration_nsecs() as f64 / 1_000_000.0,
                    reader_nsecs = guard.reader_nsecs,
                    reader_pts_seconds = nsecs_to_seconds(guard.reader_nsecs),
                    cached_until_nsecs = ?guard.cached_until_nsecs(),
                    cache_end_seconds = ?guard.cached_until_nsecs().map(nsecs_to_seconds),
                    readahead_nsecs = ?guard
                        .cached_until_nsecs()
                        .map(|cached_until| cached_until.saturating_sub(guard.reader_nsecs)),
                    raw_input_rate_bytes_per_sec = ?guard.raw_input_rate(),
                    cache_pause_percent = ?guard.cache_pause_percent(),
                    cache_paused = self.control.is_cache_paused(),
                    should_pause_demux = guard.should_pause_demux(),
                    readahead_ms = guard.readahead_nsecs as f64 / 1_000_000.0,
                    generation = guard.generation,
                    seek_generation = self.control.seek_generation(),
                    "FFmpeg demux packet cache prefetch still paused"
                );
                next_prefetch_pause_log_at =
                    now.checked_add(DEMUX_PACKET_CACHE_PREFETCH_PAUSE_LOG_INTERVAL);
            }
            let (next_guard, _) = self
                .ready
                .wait_timeout(guard, DEMUX_PACKET_CACHE_WAIT_INTERVAL)
                .expect("FFmpeg demux packet cache poisoned");
            guard = next_guard;
        }
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn generation(&self) -> u64 {
        self.state
            .lock()
            .expect("FFmpeg demux packet cache poisoned")
            .generation
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn session_id(
        &self,
    ) -> PlaybackSessionId {
        self.state
            .lock()
            .expect("FFmpeg demux packet cache poisoned")
            .session_id
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn selected_streams(
        &self,
    ) -> DemuxSelectedStreams {
        self.state
            .lock()
            .expect("FFmpeg demux packet cache poisoned")
            .selected_streams
    }
}
