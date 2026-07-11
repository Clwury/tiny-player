use std::time::Instant;

use super::{
    BackendEvent, BackendEventKind, DEMUX_PACKET_CACHE_STALL_LOG_AFTER,
    DEMUX_PACKET_CACHE_STALL_LOG_INTERVAL, DEMUX_PACKET_CACHE_WAIT_INTERVAL, DemuxPacketCache,
    DemuxSeekResult, PlaybackCacheConfig, PlaybackSeekMode, PlaybackSessionId, nsecs_to_seconds,
    seconds_to_nsecs,
};

impl DemuxPacketCache {
    pub(in crate::player::backend::ffmpeg::playback_loop) fn set_playback_recovery_demand(
        &self,
        critical: bool,
        video_required: bool,
        audio_required: bool,
    ) {
        self.shared
            .set_playback_recovery_demand(critical, video_required, audio_required);
    }

    pub(in crate::player::backend::ffmpeg::playback_loop) fn seek(
        &self,
        position_seconds: f64,
        mode: PlaybackSeekMode,
        session_id: PlaybackSessionId,
        seek_generation: u64,
    ) -> DemuxSeekResult {
        let position_seconds = position_seconds.max(0.0);
        let target_nsecs = seconds_to_nsecs(position_seconds);
        let (result, should_enter_initial_cache_pause, cache_snapshot, buffered_changed) = {
            let mut guard = self
                .shared
                .state
                .lock()
                .expect("FFmpeg demux packet cache poisoned");
            guard.error = None;
            if let Some(hit) = guard.seek_cached_with_generation_hit(
                target_nsecs,
                mode,
                session_id,
                seek_generation,
            ) {
                let buffered_until = nsecs_to_seconds(hit.buffered_until_nsecs);
                let audio_reader_head = guard
                    .selected_streams
                    .audio_stream
                    .and_then(|stream| hit.reader_heads.get(&stream.index).copied());
                tracing::debug!(
                    ?session_id,
                    position_seconds,
                    ?mode,
                    target_nsecs,
                    cached_seek_target_nsecs = hit.target_nsecs,
                    anchor_nsecs = hit.anchor_nsecs,
                    anchor_packet_id = hit.anchor_packet_id,
                    video_reader_head = hit.video_reader_head,
                    ?audio_reader_head,
                    anchor_is_recovery_point = hit.anchor_is_recovery_point,
                    anchor_is_safe_seek_point = hit.anchor_is_safe_seek_point,
                    cached_seek_preroll_nsecs = guard.cached_seek_preroll_nsecs,
                    requires_precise_trim = hit.requires_precise_trim,
                    seek_generation,
                    buffered_until,
                    read_index = guard.read_index,
                    generation = guard.generation,
                    "FFmpeg demux packet cache seek hit"
                );
                let cache_snapshot =
                    guard.cache_report_snapshot(self.shared.control.is_cache_paused());
                let buffered_changed =
                    guard.take_buffered_changed_for_cache_end(cache_snapshot.cache_end());
                guard.record_cache_state_emit(Instant::now());
                guard.record_emitted_seekable_ranges(cache_snapshot.seekable_ranges().clone());
                self.shared.refresh_monitor_snapshot(&guard);
                (
                    DemuxSeekResult::Cached,
                    false,
                    cache_snapshot,
                    buffered_changed,
                )
            } else {
                guard.request_seek(position_seconds, session_id, seek_generation, target_nsecs);
                tracing::debug!(
                    ?session_id,
                    position_seconds,
                    ?mode,
                    target_nsecs,
                    seek_generation,
                    generation = guard.generation,
                    "FFmpeg demux packet cache seek miss; requested low-level seek"
                );
                let cache_snapshot =
                    guard.cache_report_snapshot(self.shared.control.is_cache_paused());
                let buffered_changed =
                    guard.take_buffered_changed_for_cache_end(cache_snapshot.cache_end());
                guard.record_cache_state_emit(Instant::now());
                guard.record_emitted_seekable_ranges(cache_snapshot.seekable_ranges().clone());
                self.shared.refresh_monitor_snapshot(&guard);
                (
                    DemuxSeekResult::Requested,
                    guard.cache_pause_initial,
                    cache_snapshot,
                    buffered_changed,
                )
            }
        };
        let cache_state = cache_snapshot.into_cache_state();
        self.shared.ready.notify_all();
        self.shared
            .send_cache_state_events(session_id, cache_state, buffered_changed);
        if should_enter_initial_cache_pause {
            self.shared.enter_initial_cache_pause_if_needed();
        }
        result
    }

    pub(in crate::player::backend::ffmpeg::playback_loop) fn seek_low_level(
        &self,
        position_seconds: f64,
        session_id: PlaybackSessionId,
        seek_generation: u64,
        reason: &'static str,
    ) -> DemuxSeekResult {
        let position_seconds = position_seconds.max(0.0);
        let target_nsecs = seconds_to_nsecs(position_seconds);
        let (cache_snapshot, buffered_changed, should_enter_initial_cache_pause) = {
            let mut guard = self
                .shared
                .state
                .lock()
                .expect("FFmpeg demux packet cache poisoned");
            guard.error = None;
            guard.request_seek(position_seconds, session_id, seek_generation, target_nsecs);
            tracing::debug!(
                ?session_id,
                position_seconds,
                target_nsecs,
                seek_generation,
                reason,
                generation = guard.generation,
                "FFmpeg demux packet cache forced low-level seek"
            );
            let cache_snapshot = guard.cache_report_snapshot(self.shared.control.is_cache_paused());
            let buffered_changed =
                guard.take_buffered_changed_for_cache_end(cache_snapshot.cache_end());
            guard.record_cache_state_emit(Instant::now());
            guard.record_emitted_seekable_ranges(cache_snapshot.seekable_ranges().clone());
            self.shared.refresh_monitor_snapshot(&guard);
            (cache_snapshot, buffered_changed, guard.cache_pause_initial)
        };
        let cache_state = cache_snapshot.into_cache_state();
        self.shared.ready.notify_all();
        self.shared
            .send_cache_state_events(session_id, cache_state, buffered_changed);
        if should_enter_initial_cache_pause {
            self.shared.enter_initial_cache_pause_if_needed();
        }
        DemuxSeekResult::Requested
    }

    pub(in crate::player::backend::ffmpeg::playback_loop) fn shutdown(&self) {
        let mut guard = self
            .shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        guard.shutdown = true;
        self.shared.ready.notify_all();
    }

    pub(in crate::player::backend::ffmpeg::playback_loop) fn apply_cache_config(
        &self,
        cache_config: PlaybackCacheConfig,
    ) {
        let emit = {
            let mut guard = self
                .shared
                .state
                .lock()
                .expect("FFmpeg demux packet cache poisoned");
            let had_cache_buffering = guard.cache_buffering_percent.is_some();
            guard.apply_cache_config(cache_config);
            if !guard.cache_pause_enabled {
                let changed = self.shared.control.is_cache_paused()
                    && self.shared.control.set_cache_paused(false);
                if had_cache_buffering {
                    let _ = self.shared.event_tx.send(BackendEvent::new(
                        guard.session_id,
                        BackendEventKind::CacheBufferingChanged(None),
                    ));
                }
                if changed {
                    let _ = self.shared.event_tx.send(BackendEvent::new(
                        guard.session_id,
                        BackendEventKind::PausedForCacheChanged(false),
                    ));
                    let _ = self.shared.event_tx.send(BackendEvent::new(
                        guard.session_id,
                        BackendEventKind::Pause(self.shared.control.is_paused()),
                    ));
                }
            }
            self.shared.refresh_cache_pause(&mut guard);
            let emit = self.shared.prepare_cache_state_emit(&mut guard);
            self.shared.ready.notify_all();
            emit
        };
        self.shared.send_cache_state_emit(emit.into_emit());
    }

    pub(in crate::player::backend::ffmpeg::playback_loop) fn wait_until_initial_cache_fill(
        &self,
    ) -> std::result::Result<(), String> {
        let mut guard = self
            .shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        let wait_started_at = Instant::now();
        let mut next_initial_wait_log_at =
            wait_started_at.checked_add(DEMUX_PACKET_CACHE_STALL_LOG_AFTER);
        loop {
            if guard.shutdown || self.shared.control.should_stop() {
                return Ok(());
            }
            if self.shared.control.has_pending_seek() {
                return Ok(());
            }
            if let Some(error) = guard.error.clone() {
                return Err(error);
            }
            if guard.initial_cache_fill_complete() {
                return Ok(());
            }
            let now = Instant::now();
            if next_initial_wait_log_at.is_some_and(|deadline| now >= deadline) {
                tracing::debug!(
                    session_id = ?guard.session_id,
                    waited_ms = now.saturating_duration_since(wait_started_at).as_millis(),
                    read_index = guard.read_index,
                    packet_count = guard.read_range().global_order.len(),
                    cached_bytes = guard.cached_bytes,
                    forward_bytes = guard.forward_bytes(),
                    forward_duration_ms = guard.forward_duration_nsecs() as f64 / 1_000_000.0,
                    reader_nsecs = guard.reader_nsecs,
                    reader_pts_seconds = nsecs_to_seconds(guard.reader_nsecs),
                    cached_until_nsecs = ?guard.cached_until_nsecs(),
                    cache_end_seconds = ?guard.cached_until_nsecs().map(nsecs_to_seconds),
                    raw_input_rate_bytes_per_sec = ?guard.raw_input_rate(),
                    cache_pause_percent = ?guard.cache_pause_percent(),
                    cache_paused = self.shared.control.is_cache_paused(),
                    should_pause_demux = guard.should_pause_demux(),
                    readahead_ms = guard.readahead_nsecs as f64 / 1_000_000.0,
                    cache_pause_wait_ms = guard.cache_pause_wait_nsecs as f64 / 1_000_000.0,
                    "still waiting for initial FFmpeg demux cache fill"
                );
                next_initial_wait_log_at = now.checked_add(DEMUX_PACKET_CACHE_STALL_LOG_INTERVAL);
            }
            let (next_guard, _) = self
                .shared
                .ready
                .wait_timeout(guard, DEMUX_PACKET_CACHE_WAIT_INTERVAL)
                .expect("FFmpeg demux packet cache poisoned");
            guard = next_guard;
        }
    }
}

impl Drop for DemuxPacketCache {
    fn drop(&mut self) {
        self.shutdown();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
