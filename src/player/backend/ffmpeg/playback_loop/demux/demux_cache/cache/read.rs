use std::{
    os::raw::c_int,
    sync::MutexGuard,
    thread,
    time::{Duration, Instant},
};

use super::{
    DEMUX_PACKET_CACHE_STALL_LOG_AFTER, DEMUX_PACKET_CACHE_STALL_LOG_INTERVAL,
    DEMUX_PACKET_CACHE_WAIT_INTERVAL, DemuxCacheLockWait, DemuxPacketCache,
    DemuxPacketCacheMonitorSnapshot, DemuxPacketCacheReadTiming, DemuxPacketCacheState,
    DemuxPacketQueueSnapshot, DemuxReadResult, DemuxReaderWatermark, DemuxSelectedStreams,
    DemuxStreamReaderRealignResult, StreamInfo, demux_cache_blocked_on, nsecs_to_seconds,
};

impl DemuxPacketCache {
    #[allow(dead_code)]
    pub(in crate::player::backend::ffmpeg::playback_loop) fn poll_packet(
        &self,
        stream_index: c_int,
    ) -> DemuxReadResult {
        self.poll_packet_round_robin(&[stream_index]).0
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn read_packet_round_robin(
        &self,
        stream_indices: &[c_int],
    ) -> (DemuxReadResult, Option<usize>) {
        let (result, stream_offset, _) = self.read_packet_round_robin_inner(
            stream_indices,
            true,
            DemuxCacheLockWait::None,
            false,
        );
        (result, stream_offset)
    }

    pub(in crate::player::backend::ffmpeg::playback_loop) fn poll_packet_round_robin(
        &self,
        stream_indices: &[c_int],
    ) -> (DemuxReadResult, Option<usize>) {
        let (result, stream_offset, _) = self.poll_packet_round_robin_with_timing(stream_indices);
        (result, stream_offset)
    }

    pub(in crate::player::backend::ffmpeg::playback_loop) fn poll_packet_round_robin_with_timing(
        &self,
        stream_indices: &[c_int],
    ) -> (DemuxReadResult, Option<usize>, DemuxPacketCacheReadTiming) {
        self.read_packet_round_robin_inner(stream_indices, false, DemuxCacheLockWait::None, false)
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn read_available_packet_round_robin_with_lock_wait(
        &self,
        stream_indices: &[c_int],
        lock_wait: Duration,
    ) -> (DemuxReadResult, Option<usize>) {
        self.read_available_packet_round_robin_with_cache_pause_signal(
            stream_indices,
            lock_wait,
            false,
        )
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn read_available_packet_round_robin_with_cache_pause_signal(
        &self,
        stream_indices: &[c_int],
        lock_wait: Duration,
        cache_pause_signal: bool,
    ) -> (DemuxReadResult, Option<usize>) {
        let (result, stream_offset, _) = self
            .read_available_packet_round_robin_with_cache_pause_signal_and_timing(
                stream_indices,
                lock_wait,
                cache_pause_signal,
            );
        (result, stream_offset)
    }

    pub(in crate::player::backend::ffmpeg::playback_loop) fn read_available_packet_round_robin_with_cache_pause_signal_and_timing(
        &self,
        stream_indices: &[c_int],
        lock_wait: Duration,
        cache_pause_signal: bool,
    ) -> (DemuxReadResult, Option<usize>, DemuxPacketCacheReadTiming) {
        self.read_packet_round_robin_inner(
            stream_indices,
            false,
            DemuxCacheLockWait::Bounded(lock_wait),
            cache_pause_signal,
        )
    }

    pub(in crate::player::backend::ffmpeg::playback_loop) fn drain_available_packet_round_robin_with_unbounded_lock_and_timing(
        &self,
        stream_indices: &[c_int],
        cache_pause_signal: bool,
    ) -> (DemuxReadResult, Option<usize>, DemuxPacketCacheReadTiming) {
        self.read_packet_round_robin_inner(
            stream_indices,
            false,
            DemuxCacheLockWait::Unbounded,
            cache_pause_signal,
        )
    }

    pub(in crate::player::backend::ffmpeg::playback_loop) fn packet_queue_snapshot(
        &self,
    ) -> DemuxPacketQueueSnapshot {
        let guard = self
            .shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        guard.packet_queue_snapshot()
    }

    pub(in crate::player::backend::ffmpeg::playback_loop) fn repair_reader_heads_for_read_index(
        &self,
        reason: &'static str,
    ) -> bool {
        let mut guard = self
            .shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        let session_id = guard.session_id;
        let before_read_index = guard.read_index;
        let before_heads = guard.reader_heads.clone();
        guard.reset_reader_heads_for_read_index();
        let repaired = guard.reader_heads != before_heads || guard.read_index != before_read_index;
        if repaired {
            tracing::debug!(
                session_id = ?session_id,
                reason,
                before_read_index,
                after_read_index = guard.read_index,
                before_reader_heads = ?before_heads,
                after_reader_heads = ?guard.reader_heads,
                "repaired FFmpeg demux packet cache reader heads"
            );
            self.shared.refresh_monitor_snapshot(&guard);
            self.shared.ready.notify_all();
        }
        repaired
    }

    pub(in crate::player::backend::ffmpeg::playback_loop) fn stream_reader_head_timeline(
        &self,
        stream_index: c_int,
    ) -> Option<(u64, Option<u64>, Option<u64>)> {
        let guard = self
            .shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        guard.stream_reader_head_timeline(stream_index)
    }

    pub(in crate::player::backend::ffmpeg::playback_loop) fn realign_stream_reader_to_timeline(
        &self,
        stream_index: c_int,
        target_timeline_nsecs: u64,
        reason: &'static str,
    ) -> Option<DemuxStreamReaderRealignResult> {
        let mut guard = self
            .shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        let result =
            guard.realign_stream_reader_to_timeline(stream_index, target_timeline_nsecs, reason);
        if result.is_some() {
            self.shared.refresh_monitor_snapshot(&guard);
            self.shared.ready.notify_all();
        }
        result
    }

    pub(in crate::player::backend::ffmpeg::playback_loop) fn cached_reader_watermark(
        &self,
    ) -> DemuxReaderWatermark {
        self.shared.cached_monitor_snapshot().reader_watermark
    }

    pub(in crate::player::backend::ffmpeg::playback_loop) fn monitor_snapshot(
        &self,
    ) -> (DemuxPacketQueueSnapshot, DemuxReaderWatermark, bool) {
        let Some(guard) = self.try_lock_state(DemuxCacheLockWait::None) else {
            let snapshot = self.shared.cached_monitor_snapshot();
            return (snapshot.packet_queue, snapshot.reader_watermark, true);
        };
        let snapshot = DemuxPacketCacheMonitorSnapshot::from_state(&guard);
        self.shared.store_monitor_snapshot(snapshot.clone());
        (snapshot.packet_queue, snapshot.reader_watermark, false)
    }

    pub(in crate::player::backend::ffmpeg::playback_loop) fn demux_read_blocked_for(
        &self,
    ) -> Option<Duration> {
        self.shared.demux_read_blocked_for()
    }

    pub(in crate::player::backend::ffmpeg::playback_loop) fn clear_cache_pause_for_decoded_resume(
        &self,
    ) {
        self.shared.clear_cache_pause_for_decoded_resume();
    }

    pub(in crate::player::backend::ffmpeg::playback_loop) fn set_selected_streams(
        &self,
        audio_stream: Option<StreamInfo>,
        subtitle_stream: Option<StreamInfo>,
    ) {
        let mut guard = self
            .shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        guard.set_selected_streams(DemuxSelectedStreams {
            audio_stream,
            subtitle_stream,
        });
        self.shared.refresh_monitor_snapshot(&guard);
        self.shared.ready.notify_all();
    }

    fn read_packet_round_robin_inner(
        &self,
        stream_indices: &[c_int],
        wait_for_data: bool,
        lock_wait: DemuxCacheLockWait,
        cache_pause_signal: bool,
    ) -> (DemuxReadResult, Option<usize>, DemuxPacketCacheReadTiming) {
        let (mut guard, mut timing) = if wait_for_data {
            let (guard, timing) = self.lock_state_unbounded_with_timing();
            (guard, timing)
        } else {
            let (guard, timing) = self.try_lock_state_with_timing(lock_wait);
            match guard {
                Some(guard) => (guard, timing),
                None => return (DemuxReadResult::WouldBlock, None, timing),
            }
        };
        let mut logged_wait = false;
        let mut wait_started_at = None;
        let mut next_stall_log_at = None;
        loop {
            if guard.shutdown || self.shared.control.should_stop() {
                return (DemuxReadResult::Interrupted, None, timing);
            }
            if let Some(error) = guard.error.clone() {
                return (DemuxReadResult::Error(error), None, timing);
            }
            if self
                .shared
                .refresh_cache_pause(&mut guard)
                .force_cache_state_report
            {
                self.shared.emit_cache_state_after_read(&mut guard, true);
            }
            let packet_source = match guard.take_packet_round_robin(stream_indices, &mut timing) {
                Ok(packet) => packet,
                Err(error) => return (DemuxReadResult::Error(error), None, timing),
            };
            if let Some(packet_source) = packet_source {
                guard.refresh_readahead_hysteresis();
                self.shared
                    .refresh_monitor_snapshot_with_timing(&guard, &mut timing);
                let seekable_changed = guard.seekable_ranges_changed_since_last_emit();
                self.shared
                    .emit_cache_state_after_read(&mut guard, seekable_changed);
                self.shared.ready.notify_all();
                drop(guard);
                let (packet, stream_offset) = match packet_source.packet_ref(&mut timing) {
                    Ok(packet) => packet,
                    Err(error) => return (DemuxReadResult::Error(error), None, timing),
                };
                return (DemuxReadResult::Packet(packet), Some(stream_offset), timing);
            }
            let activate_started_at = Instant::now();
            if guard.activate_detached_append_range() {
                timing.refresh_reader_tracking += activate_started_at.elapsed();
                self.shared.ready.notify_all();
                continue;
            }
            if self.shared.control.is_cache_paused() && !guard.cache_pause_recovered() {
                if !wait_for_data {
                    return (DemuxReadResult::WouldBlock, None, timing);
                }
                let wait_started_at = Instant::now();
                let (next_guard, _) = self
                    .shared
                    .ready
                    .wait_timeout(guard, DEMUX_PACKET_CACHE_WAIT_INTERVAL)
                    .expect("FFmpeg demux packet cache poisoned");
                timing.data_wait += wait_started_at.elapsed();
                timing.data_waits = timing.data_waits.saturating_add(1);
                guard = next_guard;
                continue;
            }
            if guard.read_range_eof() {
                return (DemuxReadResult::Eof, None, timing);
            }
            if guard.demux_position_detached {
                let session_id = guard.session_id;
                let seek_generation = self.shared.control.seek_generation();
                let continuation_seconds = nsecs_to_seconds(guard.reader_nsecs);
                guard.request_continuation_seek(seek_generation);
                tracing::debug!(
                    ?session_id,
                    position_seconds = continuation_seconds,
                    seek_generation,
                    generation = guard.generation,
                    "FFmpeg demux packet cache exhausted selected stream queues; requested low-level continuation seek"
                );
                let emit = self.shared.prepare_cache_state_emit(&mut guard);
                self.shared.ready.notify_all();
                drop(guard);
                self.shared.send_cache_state_emit(emit.into_emit());
                let Some(next_guard) =
                    self.reacquire_read_state(wait_for_data, lock_wait, &mut timing)
                else {
                    return (DemuxReadResult::WouldBlock, None, timing);
                };
                guard = next_guard;
                continue;
            }
            self.shared
                .enter_cache_pause_if_needed(&mut guard, cache_pause_signal);
            if !logged_wait && !self.shared.control.is_cache_paused() && guard.has_demux_underrun()
            {
                self.shared.emit_cache_state_after_read(&mut guard, true);
            }
            if self.shared.should_log_would_block_diagnostic() {
                guard.log_would_block_diagnostic(stream_indices);
            }
            if !wait_for_data {
                return (DemuxReadResult::WouldBlock, None, timing);
            }
            let now = Instant::now();
            let wait_started = *wait_started_at.get_or_insert(now);
            if !logged_wait {
                let cache_paused = self.shared.control.is_cache_paused();
                let queue_snapshot = guard.packet_queue_snapshot();
                tracing::trace!(
                    session_id = ?guard.session_id,
                    blocked_on = demux_cache_blocked_on(&guard, cache_paused),
                    streams = ?stream_indices,
                    queued_packets = queue_snapshot.total_packets,
                    queued_bytes = queue_snapshot.total_bytes,
                    cached_bytes = guard.cached_bytes,
                    forward_bytes = guard.forward_bytes(),
                    forward_duration_ms = guard.forward_duration_nsecs() as f64 / 1_000_000.0,
                    reader_nsecs = guard.reader_nsecs,
                    reader_pts_seconds = nsecs_to_seconds(guard.reader_nsecs),
                    cached_until_nsecs = ?guard.cached_until_nsecs(),
                    cache_end_seconds = ?guard.cached_until_nsecs().map(nsecs_to_seconds),
                    raw_input_rate_bytes_per_sec = ?guard.raw_input_rate(),
                    cache_pause_percent = ?guard.cache_pause_percent(),
                    cache_paused,
                    generation = guard.generation,
                    seek_generation = self.shared.control.seek_generation(),
                    pending_seek = self.shared.control.has_pending_seek(),
                    should_pause_demux = guard.should_pause_demux(),
                    "waiting for FFmpeg demux per-stream packet queues"
                );
                logged_wait = true;
                next_stall_log_at = now.checked_add(DEMUX_PACKET_CACHE_STALL_LOG_AFTER);
            } else if next_stall_log_at.is_some_and(|deadline| now >= deadline) {
                let cache_paused = self.shared.control.is_cache_paused();
                let queue_snapshot = guard.packet_queue_snapshot();
                tracing::debug!(
                    session_id = ?guard.session_id,
                    blocked_on = demux_cache_blocked_on(&guard, cache_paused),
                    waited_ms = now.saturating_duration_since(wait_started).as_millis(),
                    streams = ?stream_indices,
                    queued_packets = queue_snapshot.total_packets,
                    queued_bytes = queue_snapshot.total_bytes,
                    cached_bytes = guard.cached_bytes,
                    forward_bytes = guard.forward_bytes(),
                    forward_duration_ms = guard.forward_duration_nsecs() as f64 / 1_000_000.0,
                    reader_nsecs = guard.reader_nsecs,
                    reader_pts_seconds = nsecs_to_seconds(guard.reader_nsecs),
                    cached_until_nsecs = ?guard.cached_until_nsecs(),
                    cache_end_seconds = ?guard.cached_until_nsecs().map(nsecs_to_seconds),
                    raw_input_rate_bytes_per_sec = ?guard.raw_input_rate(),
                    cache_pause_percent = ?guard.cache_pause_percent(),
                    cache_paused,
                    generation = guard.generation,
                    seek_generation = self.shared.control.seek_generation(),
                    pending_seek = self.shared.control.has_pending_seek(),
                    should_pause_demux = guard.should_pause_demux(),
                    "still waiting for FFmpeg demux per-stream packet queues"
                );
                next_stall_log_at = now.checked_add(DEMUX_PACKET_CACHE_STALL_LOG_INTERVAL);
            }
            let wait_started_at = Instant::now();
            let (next_guard, _) = self
                .shared
                .ready
                .wait_timeout(guard, DEMUX_PACKET_CACHE_WAIT_INTERVAL)
                .expect("FFmpeg demux packet cache poisoned");
            timing.data_wait += wait_started_at.elapsed();
            timing.data_waits = timing.data_waits.saturating_add(1);
            guard = next_guard;
        }
    }

    fn lock_state_unbounded_with_timing(
        &self,
    ) -> (
        MutexGuard<'_, DemuxPacketCacheState>,
        DemuxPacketCacheReadTiming,
    ) {
        let started_at = Instant::now();
        let guard = self
            .shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        (
            guard,
            DemuxPacketCacheReadTiming {
                lock_wait: started_at.elapsed(),
                ..DemuxPacketCacheReadTiming::default()
            },
        )
    }

    fn try_lock_state(
        &self,
        lock_wait: DemuxCacheLockWait,
    ) -> Option<MutexGuard<'_, DemuxPacketCacheState>> {
        self.try_lock_state_with_timing(lock_wait).0
    }

    fn try_lock_state_with_timing(
        &self,
        lock_wait: DemuxCacheLockWait,
    ) -> (
        Option<MutexGuard<'_, DemuxPacketCacheState>>,
        DemuxPacketCacheReadTiming,
    ) {
        let started_at = Instant::now();
        let mut timing = DemuxPacketCacheReadTiming::default();
        match lock_wait {
            DemuxCacheLockWait::None => {
                let guard = self.try_lock_state_once(&mut timing);
                timing.lock_wait = started_at.elapsed();
                if guard.is_none() {
                    timing.lock_timed_out = true;
                }
                (guard, timing)
            }
            DemuxCacheLockWait::Bounded(lock_wait) => {
                let deadline = Instant::now().checked_add(lock_wait);
                loop {
                    if let Some(guard) = self.try_lock_state_once(&mut timing) {
                        timing.lock_wait = started_at.elapsed();
                        return (Some(guard), timing);
                    }
                    if deadline.is_none_or(|deadline| Instant::now() >= deadline) {
                        timing.lock_wait = started_at.elapsed();
                        timing.lock_timed_out = true;
                        return (None, timing);
                    }
                    thread::yield_now();
                }
            }
            DemuxCacheLockWait::Unbounded => {
                let (guard, lock_timing) = self.lock_state_unbounded_with_timing();
                timing.lock_wait = lock_timing.lock_wait;
                (Some(guard), timing)
            }
        }
    }

    fn try_lock_state_once(
        &self,
        timing: &mut DemuxPacketCacheReadTiming,
    ) -> Option<MutexGuard<'_, DemuxPacketCacheState>> {
        match self.shared.state.try_lock() {
            Ok(guard) => Some(guard),
            Err(std::sync::TryLockError::WouldBlock) => {
                timing.try_lock_failures = timing.try_lock_failures.saturating_add(1);
                None
            }
            Err(std::sync::TryLockError::Poisoned(_)) => {
                panic!("FFmpeg demux packet cache poisoned")
            }
        }
    }

    fn reacquire_read_state(
        &self,
        wait_for_data: bool,
        lock_wait: DemuxCacheLockWait,
        timing: &mut DemuxPacketCacheReadTiming,
    ) -> Option<MutexGuard<'_, DemuxPacketCacheState>> {
        let (guard, lock_timing) = if wait_for_data {
            let (guard, lock_timing) = self.lock_state_unbounded_with_timing();
            (Some(guard), lock_timing)
        } else {
            self.try_lock_state_with_timing(lock_wait)
        };
        timing.lock_wait += lock_timing.lock_wait;
        timing.try_lock_failures = timing
            .try_lock_failures
            .saturating_add(lock_timing.try_lock_failures);
        timing.lock_timed_out |= lock_timing.lock_timed_out;
        guard
    }
}
