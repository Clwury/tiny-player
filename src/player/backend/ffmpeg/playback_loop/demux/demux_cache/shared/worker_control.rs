use std::{
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

use super::{
    DEMUX_CACHE_CONSUMER_LOCK_PRESSURE_AFTER, DEMUX_CACHE_CONSUMER_PRIORITY_HOLD,
    DEMUX_PACKET_CACHE_PREFETCH_PAUSE_LOG_AFTER, DEMUX_PACKET_CACHE_PREFETCH_PAUSE_LOG_INTERVAL,
    DEMUX_PACKET_CACHE_WAIT_INTERVAL, DEMUX_PACKET_RECOVERY_DEMAND_DIAG_INTERVAL,
    DEMUX_PACKET_RECOVERY_YIELD_MAX_WAIT, DemuxPacketCacheShared, DemuxSeekRequest,
    DemuxSelectedStreams, PlaybackSessionId, duration_nsecs, nsecs_to_seconds,
};

const PLAYBACK_RECOVERY_DEMAND_VIDEO: u8 = 1 << 0;
const PLAYBACK_RECOVERY_DEMAND_AUDIO: u8 = 1 << 1;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PlaybackRecoveryDemand {
    video_required: bool,
    audio_required: bool,
}

impl PlaybackRecoveryDemand {
    fn from_bits(bits: u8) -> Self {
        Self {
            video_required: bits & PLAYBACK_RECOVERY_DEMAND_VIDEO != 0,
            audio_required: bits & PLAYBACK_RECOVERY_DEMAND_AUDIO != 0,
        }
    }

    fn bits(self) -> u8 {
        (u8::from(self.video_required) * PLAYBACK_RECOVERY_DEMAND_VIDEO)
            | (u8::from(self.audio_required) * PLAYBACK_RECOVERY_DEMAND_AUDIO)
    }

    fn has_required_streams(self) -> bool {
        self.video_required || self.audio_required
    }
}

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
        let mut recovery_yield_started_at = None;
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

            let recovery_demand = self.playback_recovery_demand();
            let recovery_critical = self.playback_recovery_critical();
            let any_consumer_drainable = guard.consumer_drainable_packet_available();
            let recovery_demand_drainable = guard.consumer_drainable_for_recovery_demand(
                recovery_demand.video_required,
                recovery_demand.audio_required,
            );
            if recovery_critical
                && recovery_demand.has_required_streams()
                && recovery_demand_drainable
            {
                let now = Instant::now();
                let yield_started_at = *recovery_yield_started_at.get_or_insert(now);
                if now.saturating_duration_since(yield_started_at)
                    < DEMUX_PACKET_RECOVERY_YIELD_MAX_WAIT
                {
                    let (next_guard, _) = self
                        .ready
                        .wait_timeout(guard, DEMUX_PACKET_CACHE_WAIT_INTERVAL)
                        .expect("FFmpeg demux packet cache poisoned");
                    guard = next_guard;
                    continue;
                }
                if !guard.should_pause_demux() {
                    if self.should_log_recovery_demand_diagnostic() {
                        tracing::debug!(
                            session_id = ?guard.session_id,
                            recovery_critical,
                            recovery_yield_ms = now
                                .saturating_duration_since(yield_started_at)
                                .as_secs_f64()
                                * 1000.0,
                            recovery_yield_max_ms =
                                DEMUX_PACKET_RECOVERY_YIELD_MAX_WAIT.as_secs_f64() * 1000.0,
                            requested_streams = ?guard.recovery_demand_streams(
                                recovery_demand.video_required,
                                recovery_demand.audio_required,
                            ),
                            drainable_streams = ?guard.drainable_streams(),
                            cached_bytes = guard.cached_bytes,
                            forward_bytes = guard.forward_bytes(),
                            should_pause_demux = false,
                            "forcing bounded FFmpeg demux producer progress after recovery yield"
                        );
                    }
                    return None;
                }
            } else {
                recovery_yield_started_at = None;
                if recovery_critical
                    && recovery_demand.has_required_streams()
                    && any_consumer_drainable
                    && self.should_log_recovery_demand_diagnostic()
                {
                    let requested_streams = guard.recovery_demand_streams(
                        recovery_demand.video_required,
                        recovery_demand.audio_required,
                    );
                    let drainable_streams = guard.drainable_streams();
                    let missing_streams = requested_streams
                        .iter()
                        .copied()
                        .filter(|stream_index| !drainable_streams.contains(stream_index))
                        .collect::<Vec<_>>();
                    tracing::debug!(
                        session_id = ?guard.session_id,
                        recovery_critical,
                        requested_streams = ?requested_streams,
                        drainable_streams = ?drainable_streams,
                        missing_streams = ?missing_streams,
                        cached_bytes = guard.cached_bytes,
                        forward_bytes = guard.forward_bytes(),
                        should_pause_demux = guard.should_pause_demux(),
                        "bypassing FFmpeg demux recovery yield for missing demanded streams"
                    );
                }
            }

            let consumer_priority_drainable = if recovery_demand.has_required_streams() {
                recovery_demand_drainable
            } else {
                any_consumer_drainable
            };
            if self.consumer_priority_active() && consumer_priority_drainable {
                let (next_guard, _) = self
                    .ready
                    .wait_timeout(guard, DEMUX_PACKET_CACHE_WAIT_INTERVAL)
                    .expect("FFmpeg demux packet cache poisoned");
                guard = next_guard;
                continue;
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

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn consumer_priority_active(
        &self,
    ) -> bool {
        if self.consumer_waiting_readers.load(Ordering::Acquire) > 0 {
            return true;
        }
        let now = duration_nsecs(self.clock_start.elapsed());
        now < self
            .consumer_lock_pressure_until_nanos
            .load(Ordering::Acquire)
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn playback_recovery_critical(
        &self,
    ) -> bool {
        self.control.is_output_rebuffer_paused()
            || self.playback_recovery_critical.load(Ordering::Acquire)
    }

    fn playback_recovery_demand(&self) -> PlaybackRecoveryDemand {
        PlaybackRecoveryDemand::from_bits(self.playback_recovery_demand.load(Ordering::Acquire))
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn set_playback_recovery_demand(
        &self,
        critical: bool,
        video_required: bool,
        audio_required: bool,
    ) {
        let demand = PlaybackRecoveryDemand {
            video_required,
            audio_required,
        };
        let critical_changed = self
            .playback_recovery_critical
            .swap(critical, Ordering::AcqRel)
            != critical;
        let demand_changed = self
            .playback_recovery_demand
            .swap(demand.bits(), Ordering::AcqRel)
            != demand.bits();
        if critical_changed || demand_changed {
            self.ready.notify_all();
        }
    }

    fn should_log_recovery_demand_diagnostic(&self) -> bool {
        let now = duration_nsecs(self.clock_start.elapsed());
        let last = self.last_recovery_demand_diag_nanos.load(Ordering::Relaxed);
        if now.saturating_sub(last) < duration_nsecs(DEMUX_PACKET_RECOVERY_DEMAND_DIAG_INTERVAL) {
            return false;
        }
        self.last_recovery_demand_diag_nanos
            .store(now, Ordering::Relaxed);
        true
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn note_consumer_lock_wait(
        &self,
        waited: Duration,
    ) {
        if waited < DEMUX_CACHE_CONSUMER_LOCK_PRESSURE_AFTER {
            return;
        }
        let until = duration_nsecs(self.clock_start.elapsed())
            .saturating_add(duration_nsecs(DEMUX_CACHE_CONSUMER_PRIORITY_HOLD));
        self.consumer_lock_pressure_until_nanos
            .fetch_max(until, Ordering::AcqRel);
        self.ready.notify_all();
    }
}
