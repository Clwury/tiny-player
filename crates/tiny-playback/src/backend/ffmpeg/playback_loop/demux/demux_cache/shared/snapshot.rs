use std::{os::raw::c_int, sync::atomic::Ordering, time::Duration};

use super::{
    DEMUX_WOULD_BLOCK_DIAG_INTERVAL, DemuxPacketCacheMonitorSnapshot, DemuxPacketCacheReadTiming,
    DemuxPacketCacheShared, DemuxPacketCacheState, duration_nsecs, nsecs_to_seconds,
};

const DEMUX_SLOW_READ_LOW_WATER_NSECS: u64 = 250_000_000;
const DEMUX_FIRST_PACKET_WARN_AFTER: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DemuxReadPhase {
    Startup,
    SeekPreroll,
    Steady,
}

impl DemuxReadPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::SeekPreroll => "seek_preroll",
            Self::Steady => "steady",
        }
    }
}

#[derive(Clone, Copy)]
struct SlowDemuxReadContext {
    stopping: bool,
    superseded: bool,
    cache_paused: bool,
    forward_duration_nsecs: u64,
    input_generation: u64,
    first_packet_wait: Option<Duration>,
    read_result: c_int,
}

impl SlowDemuxReadContext {
    fn phase(self) -> DemuxReadPhase {
        match (self.first_packet_wait, self.input_generation) {
            (None, _) => DemuxReadPhase::Steady,
            (Some(_), 0) => DemuxReadPhase::Startup,
            (Some(_), _) => DemuxReadPhase::SeekPreroll,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlowDemuxReadClass {
    Shutdown,
    Superseded,
    Startup,
    SeekPreroll,
    FirstPacketDelayed,
    CachePause,
    Buffered,
    LowWater,
}

impl SlowDemuxReadClass {
    fn as_str(self) -> &'static str {
        match self {
            Self::Shutdown => "shutdown",
            Self::Superseded => "superseded_input",
            Self::Startup => "startup_read",
            Self::SeekPreroll => "seek_preroll_read",
            Self::FirstPacketDelayed => "first_packet_delayed",
            Self::CachePause => "cache_pause",
            Self::Buffered => "buffered_consumer_coverage",
            Self::LowWater => "low_water_stall",
        }
    }

    fn should_warn(self) -> bool {
        matches!(self, Self::LowWater | Self::FirstPacketDelayed)
    }
}

fn classify_slow_demux_read(context: SlowDemuxReadContext) -> SlowDemuxReadClass {
    if context.stopping {
        SlowDemuxReadClass::Shutdown
    } else if context.superseded {
        SlowDemuxReadClass::Superseded
    } else if context.read_result >= 0
        && context
            .first_packet_wait
            .is_some_and(|wait| wait < DEMUX_FIRST_PACKET_WARN_AFTER)
    {
        if context.input_generation == 0 {
            SlowDemuxReadClass::Startup
        } else {
            SlowDemuxReadClass::SeekPreroll
        }
    } else if context.cache_paused {
        SlowDemuxReadClass::CachePause
    } else if context.forward_duration_nsecs > DEMUX_SLOW_READ_LOW_WATER_NSECS {
        SlowDemuxReadClass::Buffered
    } else if context.read_result >= 0 && context.first_packet_wait.is_some() {
        SlowDemuxReadClass::FirstPacketDelayed
    } else {
        SlowDemuxReadClass::LowWater
    }
}

impl DemuxPacketCacheShared {
    pub(in crate::backend::ffmpeg::playback_loop::demux_cache) fn should_log_would_block_diagnostic(
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

    pub(in crate::backend::ffmpeg::playback_loop::demux_cache) fn mark_demux_read_started(&self) {
        let nanos = duration_nsecs(self.clock_start.elapsed()).max(1);
        self.demux_read_started_nanos
            .store(nanos, Ordering::Release);
    }

    pub(in crate::backend::ffmpeg::playback_loop::demux_cache) fn mark_demux_read_finished(&self) {
        self.demux_read_started_nanos.store(0, Ordering::Release);
    }

    pub(in crate::backend::ffmpeg::playback_loop::demux_cache) fn demux_read_blocked_for(
        &self,
    ) -> Option<Duration> {
        let started = self.demux_read_started_nanos.load(Ordering::Acquire);
        (started != 0).then(|| {
            Duration::from_nanos(duration_nsecs(self.clock_start.elapsed()).saturating_sub(started))
        })
    }

    pub(in crate::backend::ffmpeg::playback_loop::demux_cache) fn cached_monitor_snapshot(
        &self,
    ) -> DemuxPacketCacheMonitorSnapshot {
        self.monitor_snapshot
            .lock()
            .expect("FFmpeg demux packet cache monitor snapshot poisoned")
            .clone()
    }

    pub(in crate::backend::ffmpeg::playback_loop::demux_cache) fn store_monitor_snapshot(
        &self,
        snapshot: DemuxPacketCacheMonitorSnapshot,
    ) {
        *self
            .monitor_snapshot
            .lock()
            .expect("FFmpeg demux packet cache monitor snapshot poisoned") = snapshot;
    }

    pub(in crate::backend::ffmpeg::playback_loop::demux_cache) fn refresh_monitor_snapshot(
        &self,
        state: &DemuxPacketCacheState,
    ) {
        self.store_monitor_snapshot(DemuxPacketCacheMonitorSnapshot::from_state(state));
    }

    pub(in crate::backend::ffmpeg::playback_loop::demux_cache) fn refresh_monitor_snapshot_with_timing(
        &self,
        state: &DemuxPacketCacheState,
        timing: &mut DemuxPacketCacheReadTiming,
    ) {
        self.store_monitor_snapshot(DemuxPacketCacheMonitorSnapshot::from_state_with_timing(
            state, timing,
        ));
    }

    pub(in crate::backend::ffmpeg::playback_loop::demux_cache) fn log_slow_demux_read(
        &self,
        elapsed: Duration,
        read_result: c_int,
        input_generation: u64,
        first_packet_wait: Option<Duration>,
    ) {
        let guard = self
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        let forward_duration_nsecs = guard.forward_duration_nsecs();
        let cache_paused = self.control.is_cache_paused();
        let context = SlowDemuxReadContext {
            stopping: guard.shutdown || self.control.should_stop(),
            superseded: guard.demux_input_generation != input_generation,
            cache_paused,
            forward_duration_nsecs,
            input_generation,
            first_packet_wait,
            read_result,
        };
        let phase = context.phase();
        let class = classify_slow_demux_read(context);
        if class.should_warn() {
            tracing::warn!(
                session_id = ?guard.session_id,
                read_ms = elapsed.as_secs_f64() * 1000.0,
                read_result,
                read_phase = phase.as_str(),
                first_packet_wait_ms = ?first_packet_wait.map(|wait| wait.as_secs_f64() * 1000.0),
                demux_input_generation = input_generation,
                current_demux_input_generation = guard.demux_input_generation,
                read_index = guard.read_index,
                reader_pts_seconds = nsecs_to_seconds(guard.reader_nsecs),
                forward_bytes = guard.forward_bytes(),
                forward_duration_ms = forward_duration_nsecs as f64 / 1_000_000.0,
                cached_until_seconds = ?guard.cached_until_nsecs().map(nsecs_to_seconds),
                demux_position_detached = guard.demux_position_detached,
                raw_input_rate_bps = ?guard.raw_input_rate(),
                cache_paused,
                slow_read_class = class.as_str(),
                "FFmpeg demux av_read_frame slow read"
            );
        } else {
            tracing::debug!(
                session_id = ?guard.session_id,
                read_ms = elapsed.as_secs_f64() * 1000.0,
                read_result,
                read_phase = phase.as_str(),
                first_packet_wait_ms = ?first_packet_wait.map(|wait| wait.as_secs_f64() * 1000.0),
                demux_input_generation = input_generation,
                current_demux_input_generation = guard.demux_input_generation,
                read_index = guard.read_index,
                reader_pts_seconds = nsecs_to_seconds(guard.reader_nsecs),
                forward_bytes = guard.forward_bytes(),
                forward_duration_ms = forward_duration_nsecs as f64 / 1_000_000.0,
                cached_until_seconds = ?guard.cached_until_nsecs().map(nsecs_to_seconds),
                demux_position_detached = guard.demux_position_detached,
                raw_input_rate_bps = ?guard.raw_input_rate(),
                cache_paused,
                slow_read_class = class.as_str(),
                "FFmpeg demux av_read_frame slow read"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        DEMUX_FIRST_PACKET_WARN_AFTER, DemuxReadPhase, SlowDemuxReadClass, SlowDemuxReadContext,
        classify_slow_demux_read,
    };

    fn steady_read() -> SlowDemuxReadContext {
        SlowDemuxReadContext {
            stopping: false,
            superseded: false,
            cache_paused: false,
            forward_duration_nsecs: 0,
            input_generation: 0,
            first_packet_wait: None,
            read_result: 0,
        }
    }

    #[test]
    fn slow_demux_read_warns_only_at_unexplained_low_water() {
        let context = steady_read();
        assert_eq!(
            classify_slow_demux_read(SlowDemuxReadContext {
                forward_duration_nsecs: 250_000_000,
                ..context
            }),
            SlowDemuxReadClass::LowWater
        );
        assert_eq!(
            classify_slow_demux_read(SlowDemuxReadContext {
                forward_duration_nsecs: 250_000_001,
                ..context
            }),
            SlowDemuxReadClass::Buffered
        );
        assert_eq!(
            classify_slow_demux_read(SlowDemuxReadContext {
                cache_paused: true,
                ..context
            }),
            SlowDemuxReadClass::CachePause
        );
        assert_eq!(
            classify_slow_demux_read(SlowDemuxReadContext {
                stopping: true,
                ..context
            }),
            SlowDemuxReadClass::Shutdown
        );
        assert!(SlowDemuxReadClass::LowWater.should_warn());
        assert!(!SlowDemuxReadClass::Buffered.should_warn());
        assert!(!SlowDemuxReadClass::CachePause.should_warn());
        assert!(!SlowDemuxReadClass::Shutdown.should_warn());
    }

    #[test]
    fn successful_startup_read_before_first_append_is_not_a_low_water_stall() {
        let context = SlowDemuxReadContext {
            first_packet_wait: Some(Duration::from_micros(376_788)),
            ..steady_read()
        };
        let class = classify_slow_demux_read(context);
        assert_eq!(context.phase(), DemuxReadPhase::Startup);
        assert_eq!(class, SlowDemuxReadClass::Startup);
        assert_eq!(class.as_str(), "startup_read");
        assert!(!class.should_warn());
    }

    #[test]
    fn successful_first_packet_after_input_seek_is_preroll() {
        let context = SlowDemuxReadContext {
            input_generation: 7,
            first_packet_wait: Some(Duration::from_millis(377)),
            ..steady_read()
        };
        let class = classify_slow_demux_read(context);
        assert_eq!(context.phase(), DemuxReadPhase::SeekPreroll);
        assert_eq!(class, SlowDemuxReadClass::SeekPreroll);
        assert_eq!(class.as_str(), "seek_preroll_read");
        assert!(!class.should_warn());
    }

    #[test]
    fn warmup_grace_expires_at_the_first_packet_wait_boundary() {
        for input_generation in [0, 7] {
            let context = SlowDemuxReadContext {
                input_generation,
                first_packet_wait: Some(DEMUX_FIRST_PACKET_WARN_AFTER - Duration::from_nanos(1)),
                ..steady_read()
            };
            assert!(!classify_slow_demux_read(context).should_warn());
            let class = classify_slow_demux_read(SlowDemuxReadContext {
                first_packet_wait: Some(DEMUX_FIRST_PACKET_WARN_AFTER),
                ..context
            });
            assert_eq!(class, SlowDemuxReadClass::FirstPacketDelayed);
            assert!(class.should_warn());
        }
    }

    #[test]
    fn long_first_packet_wait_keeps_existing_cache_pause_and_buffered_classification() {
        let context = SlowDemuxReadContext {
            input_generation: 1,
            first_packet_wait: Some(Duration::from_secs(10)),
            ..steady_read()
        };
        assert_eq!(
            classify_slow_demux_read(SlowDemuxReadContext {
                cache_paused: true,
                ..context
            }),
            SlowDemuxReadClass::CachePause
        );
        assert_eq!(
            classify_slow_demux_read(SlowDemuxReadContext {
                forward_duration_nsecs: 250_000_001,
                ..context
            }),
            SlowDemuxReadClass::Buffered
        );
    }

    #[test]
    fn read_errors_do_not_receive_successful_warmup_grace() {
        for input_generation in [0, 7] {
            let context = SlowDemuxReadContext {
                input_generation,
                first_packet_wait: Some(Duration::from_millis(377)),
                read_result: -1,
                ..steady_read()
            };
            assert_eq!(
                classify_slow_demux_read(context),
                SlowDemuxReadClass::LowWater
            );
            assert!(classify_slow_demux_read(context).should_warn());
        }
    }

    #[test]
    fn empty_cache_after_warmup_keeps_the_steady_low_water_warning() {
        let context = SlowDemuxReadContext {
            input_generation: 7,
            ..steady_read()
        };
        assert_eq!(context.phase(), DemuxReadPhase::Steady);
        assert_eq!(
            classify_slow_demux_read(context),
            SlowDemuxReadClass::LowWater
        );
        assert!(classify_slow_demux_read(context).should_warn());
    }

    #[test]
    fn superseded_read_does_not_warn_about_the_new_input_watermark() {
        let context = SlowDemuxReadContext {
            superseded: true,
            first_packet_wait: Some(Duration::from_secs(10)),
            read_result: -1,
            ..steady_read()
        };
        let class = classify_slow_demux_read(context);
        assert_eq!(class, SlowDemuxReadClass::Superseded);
        assert!(!class.should_warn());
        assert_eq!(
            classify_slow_demux_read(SlowDemuxReadContext {
                stopping: true,
                ..context
            }),
            SlowDemuxReadClass::Shutdown
        );
    }
}
