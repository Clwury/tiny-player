use std::{os::raw::c_int, sync::atomic::Ordering, time::Duration};

use super::{
    DEMUX_WOULD_BLOCK_DIAG_INTERVAL, DemuxPacketCacheMonitorSnapshot, DemuxPacketCacheReadTiming,
    DemuxPacketCacheShared, DemuxPacketCacheState, duration_nsecs, nsecs_to_seconds,
};

const DEMUX_SLOW_READ_LOW_WATER_NSECS: u64 = 250_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlowDemuxReadClass {
    Shutdown,
    OutputBackpressure,
    CachePause,
    Buffered,
    LowWater,
}

impl SlowDemuxReadClass {
    fn as_str(self) -> &'static str {
        match self {
            Self::Shutdown => "shutdown",
            Self::OutputBackpressure => "output_backpressure",
            Self::CachePause => "cache_pause",
            Self::Buffered => "buffered_consumer_coverage",
            Self::LowWater => "low_water_stall",
        }
    }

    fn should_warn(self) -> bool {
        matches!(self, Self::LowWater)
    }
}

fn classify_slow_demux_read(
    stopping: bool,
    output_backpressure_paused: bool,
    cache_paused: bool,
    forward_duration_nsecs: u64,
) -> SlowDemuxReadClass {
    if stopping {
        SlowDemuxReadClass::Shutdown
    } else if output_backpressure_paused {
        SlowDemuxReadClass::OutputBackpressure
    } else if cache_paused {
        SlowDemuxReadClass::CachePause
    } else if forward_duration_nsecs > DEMUX_SLOW_READ_LOW_WATER_NSECS {
        SlowDemuxReadClass::Buffered
    } else {
        SlowDemuxReadClass::LowWater
    }
}

impl DemuxPacketCacheShared {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn should_log_would_block_diagnostic(
        &self,
    ) -> bool {
        let now = duration_nsecs(self.clock_start.elapsed());
        let last = self.last_would_block_diag_nanos.load(Ordering::Relaxed);
        if now.saturating_sub(last) < duration_nsecs(DEMUX_WOULD_BLOCK_DIAG_INTERVAL) {
            return false;
        }
        self.last_would_block_diag_nanos
            .store(now, Ordering::Relaxed);
        true
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn mark_demux_read_started(
        &self,
    ) {
        let nanos = duration_nsecs(self.clock_start.elapsed()).max(1);
        self.demux_read_started_nanos
            .store(nanos, Ordering::Release);
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn mark_demux_read_finished(
        &self,
    ) {
        self.demux_read_started_nanos.store(0, Ordering::Release);
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn demux_read_blocked_for(
        &self,
    ) -> Option<Duration> {
        let started = self.demux_read_started_nanos.load(Ordering::Acquire);
        (started != 0).then(|| {
            Duration::from_nanos(duration_nsecs(self.clock_start.elapsed()).saturating_sub(started))
        })
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn cached_monitor_snapshot(
        &self,
    ) -> DemuxPacketCacheMonitorSnapshot {
        self.monitor_snapshot
            .lock()
            .expect("FFmpeg demux packet cache monitor snapshot poisoned")
            .clone()
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn store_monitor_snapshot(
        &self,
        snapshot: DemuxPacketCacheMonitorSnapshot,
    ) {
        *self
            .monitor_snapshot
            .lock()
            .expect("FFmpeg demux packet cache monitor snapshot poisoned") = snapshot;
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn refresh_monitor_snapshot(
        &self,
        state: &DemuxPacketCacheState,
    ) {
        self.store_monitor_snapshot(DemuxPacketCacheMonitorSnapshot::from_state(state));
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn refresh_monitor_snapshot_with_timing(
        &self,
        state: &DemuxPacketCacheState,
        timing: &mut DemuxPacketCacheReadTiming,
    ) {
        self.store_monitor_snapshot(DemuxPacketCacheMonitorSnapshot::from_state_with_timing(
            state, timing,
        ));
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn log_slow_demux_read(
        &self,
        elapsed: Duration,
        read_result: c_int,
    ) {
        let guard = self
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        let forward_duration_nsecs = guard.forward_duration_nsecs();
        let cache_paused = self.control.is_cache_paused();
        let output_backpressure_paused = self
            .output_backpressure_prefetch_paused
            .load(Ordering::Acquire);
        let class = classify_slow_demux_read(
            guard.shutdown || self.control.should_stop(),
            output_backpressure_paused,
            cache_paused,
            forward_duration_nsecs,
        );
        if class.should_warn() {
            tracing::warn!(
                session_id = ?guard.session_id,
                read_ms = elapsed.as_secs_f64() * 1000.0,
                read_result,
                read_index = guard.read_index,
                reader_pts_seconds = nsecs_to_seconds(guard.reader_nsecs),
                forward_bytes = guard.forward_bytes(),
                forward_duration_ms = forward_duration_nsecs as f64 / 1_000_000.0,
                cached_until_seconds = ?guard.cached_until_nsecs().map(nsecs_to_seconds),
                demux_position_detached = guard.demux_position_detached,
                raw_input_rate_bps = ?guard.raw_input_rate(),
                cache_paused,
                output_backpressure_paused,
                slow_read_class = class.as_str(),
                "FFmpeg demux av_read_frame slow read at low water"
            );
        } else {
            tracing::debug!(
                session_id = ?guard.session_id,
                read_ms = elapsed.as_secs_f64() * 1000.0,
                read_result,
                read_index = guard.read_index,
                reader_pts_seconds = nsecs_to_seconds(guard.reader_nsecs),
                forward_bytes = guard.forward_bytes(),
                forward_duration_ms = forward_duration_nsecs as f64 / 1_000_000.0,
                cached_until_seconds = ?guard.cached_until_nsecs().map(nsecs_to_seconds),
                demux_position_detached = guard.demux_position_detached,
                raw_input_rate_bps = ?guard.raw_input_rate(),
                cache_paused,
                output_backpressure_paused,
                slow_read_class = class.as_str(),
                "FFmpeg demux av_read_frame slow read completed without consumer starvation"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{SlowDemuxReadClass, classify_slow_demux_read};

    #[test]
    fn slow_demux_read_warns_only_at_unexplained_low_water() {
        assert_eq!(
            classify_slow_demux_read(false, false, false, 250_000_000),
            SlowDemuxReadClass::LowWater
        );
        assert_eq!(
            classify_slow_demux_read(false, false, false, 250_000_001),
            SlowDemuxReadClass::Buffered
        );
        assert_eq!(
            classify_slow_demux_read(false, true, false, 0),
            SlowDemuxReadClass::OutputBackpressure
        );
        assert_eq!(
            classify_slow_demux_read(false, false, true, 0),
            SlowDemuxReadClass::CachePause
        );
        assert_eq!(
            classify_slow_demux_read(true, false, false, 0),
            SlowDemuxReadClass::Shutdown
        );
    }
}
