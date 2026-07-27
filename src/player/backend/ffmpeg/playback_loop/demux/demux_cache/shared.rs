use std::{sync::MutexGuard, time::Duration};

pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) use super::{
    BackendEvent, BackendEventKind, CachePauseRefresh, CacheStateEmit, CachedDemuxPacket,
    DEMUX_CACHE_CONSUMER_LOCK_PRESSURE_AFTER, DEMUX_CACHE_CONSUMER_PRIORITY_HOLD,
    DEMUX_PACKET_CACHE_PREFETCH_PAUSE_LOG_AFTER, DEMUX_PACKET_CACHE_PREFETCH_PAUSE_LOG_INTERVAL,
    DEMUX_PACKET_CACHE_WAIT_INTERVAL, DEMUX_PACKET_RECOVERY_DEMAND_DIAG_INTERVAL,
    DEMUX_PACKET_RECOVERY_YIELD_MAX_WAIT, DEMUX_WOULD_BLOCK_DIAG_INTERVAL,
    DemuxCacheReportSnapshot, DemuxPacketAppendTiming, DemuxPacketCacheMonitorSnapshot,
    DemuxPacketCacheReadTiming, DemuxPacketCacheShared, DemuxPacketCacheState, DemuxSeekRequest,
    DemuxSelectedStreams, PlaybackCacheState, PlaybackSessionId, duration_nsecs,
    log_demux_packet_append_timing, nsecs_to_seconds,
};

#[path = "shared/cache_pause.rs"]
mod cache_pause;
#[path = "shared/events.rs"]
mod events;
#[path = "shared/mutation.rs"]
mod mutation;
#[path = "shared/snapshot.rs"]
mod snapshot;
#[path = "shared/worker_control.rs"]
mod worker_control;

impl DemuxPacketCacheShared {
    /// Publish cache/output availability through both the legacy cache
    /// condition variable and the playback-wide generation. New waits use the
    /// latter; the former remains for focused cache tests and compatibility.
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn notify_ready(&self) {
        self.control.wake();
        self.ready.notify_all();
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn wait_for_ready_change<
        'a,
    >(
        &'a self,
        guard: MutexGuard<'a, DemuxPacketCacheState>,
        timeout: Duration,
    ) -> MutexGuard<'a, DemuxPacketCacheState> {
        let observed_generation = self.control.wake_generation();
        drop(guard);
        if !self.control.should_interrupt() {
            self.control
                .wait_for_wake_change(observed_generation, timeout);
        }
        self.state
            .lock()
            .expect("FFmpeg demux packet cache poisoned")
    }
}
